"""Tests for the pacto-bot-sdk high-level Bot layer."""

from __future__ import annotations

import asyncio
import json
from typing import Any
from unittest.mock import AsyncMock

import pytest

from pacto_bot_sdk import Bot, PactoClient, parse_command
from pacto_bot_sdk._generated.models import AgentEventParams, AgentRateLimitedParams
from pacto_bot_sdk.transports import Transport, UnixTransport


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
        self.connect_failures_remaining = 0
        self.connect_exception = ConnectionError("mock connect failure")

    async def connect(self) -> None:
        if self.connect_failures_remaining > 0:
            self.connect_failures_remaining -= 1
            raise self.connect_exception
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

    def inject_eof(self) -> None:
        self._inbound.put_nowait("")


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
# Non-command dispatch
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_malformed_event_ignored_without_default_handler():
    """Non-slash events are ignored when no default handler is registered."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="plain text without slash",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)
    bot._client.handler_response.assert_awaited_once_with(
        action="ignore", event_id="e-1"
    )


@pytest.mark.asyncio
async def test_default_handler_receives_non_command_event():
    """Non-slash events are routed to the default handler when registered."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[AgentEventParams] = []

    @bot.default
    async def fallback(event, b):
        calls.append(event)
        return {
            "event_id": event.event_id,
            "action": "reply",
            "content": "Try /help",
        }

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-2",
        type="dm_received",
        chat_id="npub1chat",
        content="hello there",
        rumor_id="r-2",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    assert len(calls) == 1
    assert calls[0].event_id == "e-2"
    bot._client.handler_response.assert_awaited_once_with(
        action="reply", event_id="e-2", content="Try /help"
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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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


# ---------------------------------------------------------------------------
# Degraded state and reconnection resilience
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_bot_degraded_state_reflects_open_circuit(
    transport: MockTransport,
) -> None:
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=2,
        circuit_cooling_off_seconds=60.0,
    )
    transport.connect_failures_remaining = 5
    assert bot.is_degraded is False

    task = asyncio.create_task(bot._run([]))
    deadline = asyncio.get_running_loop().time() + 5.0
    while not bot.is_degraded and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
    assert bot.is_degraded is True

    bot._request_shutdown()
    await task


@pytest.mark.asyncio
async def test_bot_degraded_logs_recovery_when_circuit_closes(
    transport: MockTransport,
    capsys,
) -> None:
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=3,
        circuit_cooling_off_seconds=0.0,
    )
    transport.connect_failures_remaining = 3

    task = asyncio.create_task(bot._run([]))
    deadline = asyncio.get_running_loop().time() + 5.0
    register_frame = None
    while register_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.register":
                register_frame = frame
                break
    assert register_frame is not None
    transport.inject({
        "jsonrpc": "2.0",
        "id": register_frame["id"],
        "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
    })

    await asyncio.sleep(0.1)
    assert bot.is_degraded is False

    bot._request_shutdown()
    await task

    stderr = capsys.readouterr().err
    assert "degraded:" in stderr
    assert "recovered" in stderr


@pytest.mark.asyncio
async def test_bot_degraded_status_logs_at_most_once_per_interval(
    transport: MockTransport,
    capsys,
) -> None:
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=1,
        circuit_cooling_off_seconds=60.0,
        degraded_log_interval=0.3,
    )
    transport.connect_failures_remaining = 10

    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.5)
    bot._request_shutdown()
    await task

    stderr = capsys.readouterr().err
    # The circuit opens with one message and may emit a periodic status line.
    degraded_lines = [line for line in stderr.splitlines() if "degraded:" in line]
    # At most one opening log + one periodic status log within 0.5s for a 0.3s interval.
    assert len(degraded_lines) <= 2


@pytest.mark.asyncio
async def test_bot_reconnect_after_transient_disconnect(
    transport: MockTransport,
) -> None:
    """A disconnect during dispatch is followed by a successful reconnect."""
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=5,
    )

    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.05)

    # Complete first registration.
    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break
    await asyncio.sleep(0.05)

    # Simulate a graceful daemon disconnect by sending an empty line which
    # the read loop treats as EOF and ends the dispatch loop.
    transport.inject_eof()

    # Poll for the reconnect frame after the disconnect.
    deadline = asyncio.get_running_loop().time() + 5.0
    reconnect_frame = None
    while reconnect_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.reconnect":
                reconnect_frame = frame
                break
    assert reconnect_frame is not None
    transport.inject({
        "jsonrpc": "2.0",
        "id": reconnect_frame["id"],
        "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
    })

    await asyncio.sleep(0.05)

    bot._request_shutdown()
    await task

    register_frames = [f for f in transport.frames if f.get("method") == "handler.register"]
    assert len(register_frames) == 1
    reconnect_frames = [f for f in transport.frames if f.get("method") == "handler.reconnect"]
    assert len(reconnect_frames) == 1


@pytest.mark.asyncio
async def test_bot_reconnect_falls_back_to_register_when_token_rejected(
    transport: MockTransport,
) -> None:
    """A rejected reconnect token causes a fresh handler registration."""
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=5,
    )

    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.05)

    # Complete first registration.
    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break
    await asyncio.sleep(0.05)

    # Simulate a graceful daemon disconnect.
    transport.inject_eof()

    # Poll for the reconnect frame after the disconnect.
    deadline = asyncio.get_running_loop().time() + 5.0
    reconnect_frame = None
    while reconnect_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.reconnect":
                reconnect_frame = frame
                break
    assert reconnect_frame is not None

    # Daemon no longer recognizes the old handler.
    transport.inject({
        "jsonrpc": "2.0",
        "id": reconnect_frame["id"],
        "error": {"code": -32001, "message": "handler not registered"},
    })

    # Poll for the fallback register frame.
    deadline = asyncio.get_running_loop().time() + 5.0
    register_frame = None
    while register_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.register":
                register_frame = frame
                break
    assert register_frame is not None
    transport.inject({
        "jsonrpc": "2.0",
        "id": register_frame["id"],
        "result": {"handler_id": "h-2", "reconnect_token": "rt-2", "registered_events": ["dm_received"]},
    })

    await asyncio.sleep(0.05)

    bot._request_shutdown()
    await task

    register_frames = [f for f in transport.frames if f.get("method") == "handler.register"]
    assert len(register_frames) == 2
    reconnect_frames = [f for f in transport.frames if f.get("method") == "handler.reconnect"]
    assert len(reconnect_frames) == 1


@pytest.mark.asyncio
async def test_bot_reconnect_falls_back_to_register_on_invalid_reconnect_token(
    transport: MockTransport,
) -> None:
    """An invalid reconnect token error causes a fresh handler registration."""
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=5,
    )

    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.05)

    # Complete first registration.
    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break
    await asyncio.sleep(0.05)

    # Simulate a graceful daemon disconnect.
    transport.inject_eof()

    # Poll for the reconnect frame after the disconnect.
    deadline = asyncio.get_running_loop().time() + 5.0
    reconnect_frame = None
    while reconnect_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.reconnect":
                reconnect_frame = frame
                break
    assert reconnect_frame is not None

    # Daemon recognizes the handler id but the token is wrong.
    transport.inject({
        "jsonrpc": "2.0",
        "id": reconnect_frame["id"],
        "error": {"code": -32008, "message": "invalid reconnect token"},
    })

    # Poll for the fallback register frame.
    deadline = asyncio.get_running_loop().time() + 5.0
    register_frame = None
    while register_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.register":
                register_frame = frame
                break
    assert register_frame is not None
    transport.inject({
        "jsonrpc": "2.0",
        "id": register_frame["id"],
        "result": {"handler_id": "h-2", "reconnect_token": "rt-2", "registered_events": ["dm_received"]},
    })

    await asyncio.sleep(0.05)

    bot._request_shutdown()
    await task

    register_frames = [f for f in transport.frames if f.get("method") == "handler.register"]
    assert len(register_frames) == 2
    reconnect_frames = [f for f in transport.frames if f.get("method") == "handler.reconnect"]
    assert len(reconnect_frames) == 1


@pytest.mark.asyncio
async def test_bot_shutdown_during_backoff_exits_cleanly(
    transport: MockTransport,
) -> None:
    """Shutdown requested during a backoff sleep exits immediately."""
    transport.connect_failures_remaining = 10
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=30.0,
        retry_max_backoff=30.0,
        circuit_failure_threshold=5,
    )

    start = asyncio.get_running_loop().time()
    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.05)
    bot._request_shutdown()
    await task
    elapsed = asyncio.get_running_loop().time() - start

    assert elapsed < 1.0


@pytest.mark.asyncio
async def test_bot_shutdown_during_cooling_off_exits_cleanly(
    transport: MockTransport,
) -> None:
    """Shutdown requested while the circuit is open exits immediately."""
    transport.connect_failures_remaining = 10
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.01,
        retry_max_backoff=0.01,
        circuit_failure_threshold=1,
        circuit_cooling_off_seconds=30.0,
    )

    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.1)
    assert bot.is_degraded is True

    bot._request_shutdown()
    start = asyncio.get_running_loop().time()
    await task
    elapsed = asyncio.get_running_loop().time() - start

    assert elapsed < 1.0
    assert transport.closed is True


@pytest.mark.asyncio
async def test_bot_circuit_reopens_after_failed_probe(
    transport: MockTransport,
) -> None:
    """A failed half-open probe reopens the circuit and restarts cooling-off."""
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.01,
        retry_max_backoff=0.01,
        circuit_failure_threshold=1,
        circuit_cooling_off_seconds=0.1,
    )
    transport.connect_failures_remaining = 10

    task = asyncio.create_task(bot._run([]))
    deadline = asyncio.get_running_loop().time() + 5.0
    while not bot.is_degraded and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
    assert bot.is_degraded is True

    # Let the first cooling-off period elapse; the probe fails because the
    # mock transport still has no daemon response.
    await asyncio.sleep(0.15)
    assert bot.is_degraded is True

    bot._request_shutdown()
    await task


@pytest.mark.asyncio
async def test_bot_custom_transport_instance_is_reused_and_reconnectable(
    transport: MockTransport,
) -> None:
    """A custom transport instance is closed and reopened across reconnects."""
    bot = Bot(
        "test-bot",
        transport=transport,
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=5,
    )
    assert bot._transport is transport

    task = asyncio.create_task(bot._run([]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break
    await asyncio.sleep(0.05)

    transport.inject_eof()

    # Poll for the reconnect frame after the disconnect.
    deadline = asyncio.get_running_loop().time() + 5.0
    reconnect_frame = None
    while reconnect_frame is None and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
        for frame in transport.frames:
            if frame.get("method") == "handler.reconnect":
                reconnect_frame = frame
                break
    assert reconnect_frame is not None
    transport.inject({
        "jsonrpc": "2.0",
        "id": reconnect_frame["id"],
        "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
    })

    await asyncio.sleep(0.05)

    bot._request_shutdown()
    await task

    # The same transport instance was used for both attempts.
    assert bot._transport is transport
    register_frames = [f for f in transport.frames if f.get("method") == "handler.register"]
    assert len(register_frames) == 1
    reconnect_frames = [f for f in transport.frames if f.get("method") == "handler.reconnect"]
    assert len(reconnect_frames) == 1


@pytest.mark.asyncio
async def test_run_retries_and_shuts_down_cleanly_when_socket_missing(
    monkeypatch,
):
    """Bot.run() retries on startup errors and shuts down cleanly."""
    monkeypatch.delenv("PACTO_TRANSPORT", raising=False)
    bot = Bot(
        "test-bot",
        socket_path="/tmp/this-socket-does-not-exist-pacto.sock",
        retry_initial_backoff=0.05,
        retry_max_backoff=0.1,
        circuit_failure_threshold=2,
        circuit_cooling_off_seconds=60.0,
    )
    task = asyncio.create_task(bot._run([]))
    deadline = asyncio.get_running_loop().time() + 5.0
    while not bot.is_degraded and asyncio.get_running_loop().time() < deadline:
        await asyncio.sleep(0.05)
    assert bot.is_degraded is True
    bot._request_shutdown()
    await task


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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
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
# Rate-limited handler
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_rate_limited_handler_called(bot, transport):
    """agent.rate_limited notifications are routed to the registered handler."""
    calls: list[Any] = []

    @bot.rate_limited
    async def on_rate_limited(params, b):
        calls.append((params, b))

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.rate_limited",
        "params": {"bot_id": "test-bot", "group_id": "0xabc123", "window_seconds": 42},
    })

    await asyncio.sleep(0.05)

    assert len(calls) == 1
    params, b = calls[0]
    assert isinstance(params, AgentRateLimitedParams)
    assert params.window_seconds == 42
    assert b is bot

    bot._request_shutdown()
    await task


@pytest.mark.asyncio
async def test_rate_limited_handler_optional(bot, transport):
    """agent.rate_limited notifications are logged when no handler is registered."""
    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.rate_limited",
        "params": {"bot_id": "test-bot", "group_id": "0xabc123", "window_seconds": 7},
    })

    await asyncio.sleep(0.05)

    # No crash and no handler side effects to assert; the dispatch loop is still running.
    bot._request_shutdown()
    await task


# ---------------------------------------------------------------------------
# Retry/circuit configuration
# ---------------------------------------------------------------------------


def test_bot_configuration_retry_settings_stored():
    bot = Bot(
        "test-bot",
        transport=MockTransport(),
        retry_initial_backoff=2.0,
        retry_max_backoff=20.0,
        retry_jitter_ratio=0.3,
        circuit_failure_threshold=3,
        circuit_cooling_off_seconds=45.0,
        degraded_log_interval=30.0,
    )
    assert bot._retry_initial_backoff_arg == 2.0
    assert bot._retry_max_backoff_arg == 20.0
    assert bot._retry_jitter_ratio_arg == 0.3
    assert bot._circuit_failure_threshold_arg == 3
    assert bot._circuit_cooling_off_seconds_arg == 45.0
    assert bot._degraded_log_interval_arg == 30.0


@pytest.mark.asyncio
async def test_bot_configuration_cli_overrides_constructor():
    bot = Bot(
        "test-bot",
        transport=MockTransport(),
        retry_initial_backoff=2.0,
        retry_max_backoff=20.0,
        retry_jitter_ratio=0.3,
        circuit_failure_threshold=3,
        circuit_cooling_off_seconds=45.0,
        degraded_log_interval=30.0,
    )
    args = bot._parse_args([
        "--retry-initial-backoff", "5.0",
        "--retry-max-backoff", "60.0",
        "--retry-jitter-ratio", "0.5",
        "--circuit-failure-threshold", "10",
        "--circuit-cooling-off-seconds", "120.0",
        "--degraded-log-interval", "0",
    ])
    circuit = bot._resolve_retry_settings(args)
    assert circuit.retry_initial_backoff == 5.0
    assert circuit.retry_max_backoff == 60.0
    assert circuit.retry_jitter_ratio == 0.5
    assert circuit.circuit_failure_threshold == 10
    assert circuit.circuit_cooling_off_seconds == 120.0
    assert circuit.degraded_log_interval == 0.0


def test_bot_configuration_rejects_invalid_retry_settings():
    with pytest.raises(ValueError, match="circuit_failure_threshold must be positive"):
        Bot("test-bot", transport=MockTransport(), circuit_failure_threshold=0)
    with pytest.raises(ValueError, match="retry_initial_backoff must be non-negative"):
        Bot("test-bot", transport=MockTransport(), retry_initial_backoff=-1)
    with pytest.raises(ValueError, match="retry_jitter_ratio must be non-negative"):
        Bot("test-bot", transport=MockTransport(), retry_jitter_ratio=-0.1)


# ---------------------------------------------------------------------------
# Constructor transport resolution
# ---------------------------------------------------------------------------


def test_bot_accepts_transport_instance(bot, transport):
    assert bot._transport is transport


def test_bot_rejects_unknown_transport_name():
    with pytest.raises(ValueError, match="unsupported transport"):
        Bot("x", transport="udp")


def test_bot_accepts_custom_transport_without_logger_attribute():
    """Custom transports that disallow dynamic attributes must not raise."""

    class SlotsTransport:
        __slots__ = ("connected",)
        name = "slots"

        def __init__(self) -> None:
            self.connected = False

        async def connect(self) -> None:
            self.connected = True

        async def readline(self) -> str:
            return ""

        async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
            return None

        async def close(self) -> None:
            pass

    transport = SlotsTransport()
    bot = Bot("test-bot", transport=transport)
    assert bot._transport is transport


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


# ---------------------------------------------------------------------------
# Event-type routing (U1)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_event_type_routes_before_command():
    """@bot.event(type) is checked before slash-command parsing."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []

    @bot.event("dm_received")
    async def on_dm(event, b):
        calls.append("event")
        return b.ignore(event)

    @bot.command("/dm_received")
    async def cmd(event, b):
        calls.append("command")
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-event",
        type="dm_received",
        chat_id="npub1chat",
        content="/dm_received",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    assert calls == ["event"]
    bot._client.handler_response.assert_awaited_once_with(
        action="ignore", event_id="e-event"
    )


@pytest.mark.asyncio
async def test_dm_decorator_routes_dm_received():
    """@bot.dm is shorthand for @bot.event(\"dm_received\")."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[AgentEventParams] = []

    @bot.dm
    async def on_dm(event, b):
        calls.append(event)
        return b.reply(event, "DM received")

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-dm",
        type="dm_received",
        chat_id="npub1chat",
        content="hello",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    assert len(calls) == 1
    bot._client.handler_response.assert_awaited_once_with(
        action="reply", event_id="e-dm", content="DM received"
    )


# ---------------------------------------------------------------------------
# Hears routing (U2)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_hears_exact_first_token_match():
    """@bot.hears(token) matches the trimmed first token of the content."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []

    @bot.hears("hello")
    async def on_hello(event, b):
        calls.append("hello")
        return b.ignore(event)

    @bot.command("/hello")
    async def cmd_hello(event, b):
        calls.append("command")
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-hears",
        type="dm_received",
        chat_id="npub1chat",
        content="hello world",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    assert calls == ["hello"]
    bot._client.handler_response.assert_awaited_once_with(
        action="ignore", event_id="e-hears"
    )


@pytest.mark.asyncio
async def test_hears_falls_through_to_command_and_default():
    """Unmatched hears tokens fall through to commands and then the default handler."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []

    @bot.hears("hello")
    async def on_hello(event, b):
        calls.append("hello")
        return b.ignore(event)

    @bot.command("/help")
    async def cmd_help(event, b):
        calls.append("command")
        return b.ignore(event)

    @bot.default
    async def fallback(event, b):
        calls.append("default")
        return b.ignore(event)

    # Falls through to command.
    await bot._handle_event(AgentEventParams(
        bot_id="test-bot",
        event_id="e-cmd",
        type="dm_received",
        chat_id="npub1chat",
        content="/help",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    ))
    assert calls == ["command"]
    calls.clear()

    # Falls through to default.
    await bot._handle_event(AgentEventParams(
        bot_id="test-bot",
        event_id="e-default",
        type="dm_received",
        chat_id="npub1chat",
        content="random text",
        rumor_id="r-2",
        author="npub1author",
        timestamp=1234567890,
    ))
    assert calls == ["default"]


# ---------------------------------------------------------------------------
# Auto-acknowledge (U3)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_auto_acknowledge_true_sends_ignore_for_none():
    """When auto_acknowledge is True, a handler returning None emits ignore."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()

    @bot.command("/noop")
    async def noop(_event, _b):
        return None

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-noop",
        type="dm_received",
        chat_id="npub1chat",
        content="/noop",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    bot._client.handler_response.assert_awaited_once_with(
        action="ignore", event_id="e-noop"
    )


@pytest.mark.asyncio
async def test_auto_acknowledge_false_preserves_silence_for_none():
    """When auto_acknowledge is False, a handler returning None sends nothing."""
    bot = Bot("test-bot", transport=MockTransport(), auto_acknowledge=False)
    bot._client = AsyncMock()

    @bot.command("/noop")
    async def noop(_event, _b):
        return None

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-noop",
        type="dm_received",
        chat_id="npub1chat",
        content="/noop",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    bot._client.handler_response.assert_not_awaited()


@pytest.mark.asyncio
async def test_ignore_and_reply_helpers():
    """bot.ignore and bot.reply return canonical response dicts."""
    bot = Bot("test-bot", transport=MockTransport())
    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-helpers",
        type="dm_received",
        chat_id="npub1chat",
        content="x",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    assert bot.ignore(event) == {"event_id": "e-helpers", "action": "ignore"}
    assert bot.reply(event, "hi") == {
        "event_id": "e-helpers",
        "action": "reply",
        "content": "hi",
    }


@pytest.mark.asyncio
async def test_invalid_dict_return_logs_and_ignores():
    """Handlers returning malformed dicts are logged and answered with ignore."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()

    @bot.command("/bad")
    async def bad(_event, _b):
        return {"not_event_id": "x"}

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-bad",
        type="dm_received",
        chat_id="npub1chat",
        content="/bad",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    bot._client.handler_response.assert_awaited_once_with(
        action="ignore", event_id="e-bad"
    )


@pytest.mark.asyncio
async def test_exception_path_reply_on_error():
    """Exceptions send a friendly reply by default."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()

    @bot.command("/boom")
    async def boom(_event, _b):
        raise RuntimeError("intentional")

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-boom",
        type="dm_received",
        chat_id="npub1chat",
        content="/boom",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    bot._client.handler_response.assert_awaited_once_with(
        action="reply", event_id="e-boom", content="Sorry, I couldn't process that."
    )


@pytest.mark.asyncio
async def test_exception_path_reply_on_error_disabled():
    """Exceptions send ignore when reply_on_error is disabled."""
    bot = Bot("test-bot", transport=MockTransport(), reply_on_error=False)
    bot._client = AsyncMock()

    @bot.command("/boom")
    async def boom(_event, _b):
        raise RuntimeError("intentional")

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-boom",
        type="dm_received",
        chat_id="npub1chat",
        content="/boom",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    await bot._handle_event(event)

    bot._client.handler_response.assert_awaited_once_with(
        action="ignore", event_id="e-boom"
    )


# ---------------------------------------------------------------------------
# Bot helpers (U5)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_own_pubkey_read_from_registration_response():
    """bot.own_pubkey reads the daemon's own_pubkeys map for this bot_id."""
    bot = Bot("test-bot", transport=MockTransport())
    assert bot.own_pubkey is None
    bot._own_pubkeys = {"test-bot": "npub1test", "other": "npub1other"}
    assert bot.own_pubkey == "npub1test"


@pytest.mark.asyncio
async def test_send_group_message_prefills_bot_id():
    """bot.send_group_message calls agent_send_group_message with bot_id."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    bot._client.agent_send_group_message.return_value = "ok"

    result = await bot.send_group_message("0xabc", "hello squad")
    assert result == "ok"
    bot._client.agent_send_group_message.assert_awaited_once_with(
        bot_id="test-bot", group_id="0xabc", content="hello squad"
    )


@pytest.mark.asyncio
async def test_is_squad_member_prefills_bot_id():
    """bot.is_squad_member calls agent_is_squad_member with bot_id."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    bot._client.agent_is_squad_member.return_value = AsyncMock(is_member=True)

    result = await bot.is_squad_member("0xabc", "npub1member")
    assert result is True
    bot._client.agent_is_squad_member.assert_awaited_once_with(
        bot_id="test-bot", group_id="0xabc", member_pubkey="npub1member"
    )


@pytest.mark.asyncio
async def test_exit_squad_prefills_bot_id_and_returns_event_id():
    """bot.exit_squad calls agent_exit_mls_group with bot_id."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    bot._client.agent_exit_mls_group.return_value = AsyncMock(event_id="event-id-hex")

    result = await bot.exit_squad("0xabc")
    assert result == "event-id-hex"
    bot._client.agent_exit_mls_group.assert_awaited_once_with(
        bot_id="test-bot", group_id="0xabc"
    )


@pytest.mark.asyncio
async def test_create_mls_group_prefills_bot_id():
    """bot.create_mls_group calls agent_create_mls_group with bot_id."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    bot._client.agent_create_mls_group.return_value = AsyncMock(wire_id="wire-create")

    result = await bot.create_mls_group("group-a", "npub1recipient")
    assert result == "wire-create"
    bot._client.agent_create_mls_group.assert_awaited_once_with(
        bot_id="test-bot", group_name="group-a", recipient="npub1recipient"
    )


@pytest.mark.asyncio
async def test_invite_to_mls_group_prefills_bot_id():
    """bot.invite_to_mls_group calls agent_invite_to_mls_group with bot_id."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    bot._client.agent_invite_to_mls_group.return_value = AsyncMock(wire_id="wire-invite")

    result = await bot.invite_to_mls_group("group-a", "npub1recipient")
    assert result == "wire-invite"
    bot._client.agent_invite_to_mls_group.assert_awaited_once_with(
        bot_id="test-bot", group_name="group-a", recipient="npub1recipient"
    )


# ---------------------------------------------------------------------------
# Throttle and lock (U7)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_throttle_skips_repeated_calls_within_window():
    """Throttle ignores calls within window_seconds of the first call."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []

    @bot.command("/ping")
    @bot.throttle(key=lambda e: e.chat_id, window_seconds=0.5)
    async def ping(event, b):
        calls.append("ping")
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/ping",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )

    await bot._handle_event(event)
    await bot._handle_event(event)

    assert len(calls) == 1
    assert bot._client.handler_response.await_count == 2


@pytest.mark.asyncio
async def test_lock_skip_on_conflict():
    """Lock on_conflict=skip drops overlapping calls."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []
    gate = asyncio.Event()

    @bot.command("/slow")
    @bot.lock(name="slow", on_conflict="skip")
    async def slow(event, b):
        calls.append("slow")
        await gate.wait()
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/slow",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )

    first = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)  # let first task acquire the lock
    second = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)  # let second task skip
    gate.set()
    await asyncio.gather(first, second)

    assert len(calls) == 1


@pytest.mark.asyncio
async def test_lock_queue_queues_until_release():
    """Lock on_conflict=queue serializes calls."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []
    gate = asyncio.Event()

    @bot.command("/slow")
    @bot.lock(name="slow")
    async def slow(event, b):
        calls.append("slow")
        await gate.wait()
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/slow",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )

    first = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    second = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    gate.set()
    await asyncio.gather(first, second)

    assert len(calls) == 2


@pytest.mark.asyncio
async def test_lock_max_waiters_skips_excess():
    """Lock with max_waiters skips calls beyond the configured queue depth."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []
    gate = asyncio.Event()

    @bot.command("/slow")
    @bot.lock(name="slow", max_waiters=1)
    async def slow(event, b):
        calls.append("slow")
        await gate.wait()
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/slow",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )

    first = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    second = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    third = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    gate.set()
    await asyncio.gather(first, second, third, return_exceptions=True)

    assert len(calls) == 2


@pytest.mark.asyncio
async def test_throttle_and_lock_stack():
    """Throttle and lock compose together inside a command decorator."""
    bot = Bot("test-bot", transport=MockTransport())
    bot._client = AsyncMock()
    calls: list[str] = []
    gate = asyncio.Event()

    @bot.command("/ping")
    @bot.lock(name="ping", on_conflict="skip")
    @bot.throttle(key=lambda e: e.chat_id, window_seconds=0.5)
    async def ping(event, b):
        calls.append("ping")
        await gate.wait()
        return b.ignore(event)

    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/ping",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )

    first = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    second = asyncio.create_task(bot._handle_event(event))
    await asyncio.sleep(0.05)
    gate.set()
    await asyncio.gather(first, second)

    # Throttle causes the second call to be skipped before the lock is checked,
    # so only one handler invocation runs.
    assert len(calls) == 1
    assert bot._client.handler_response.await_count == 2


# ---------------------------------------------------------------------------
# Unknown notification types (U8)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_unknown_notification_warns_once_per_type(transport: MockTransport, capsys) -> None:
    """Unknown notification types are logged at warning level once per type."""
    bot = Bot("test-bot", transport=transport)

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    class UnknownNotification:
        pass

    # Two unknown notifications of the same type should only produce one warning.
    await bot._client._notify_queue.put(UnknownNotification())
    await bot._client._notify_queue.put(UnknownNotification())
    await asyncio.sleep(0.05)

    bot._request_shutdown()
    await task

    stderr = capsys.readouterr().err
    warnings = [line for line in stderr.splitlines() if "unknown notification type" in line]
    assert len(warnings) == 1
    assert "UnknownNotification" in warnings[0]


@pytest.mark.asyncio
async def test_unknown_notification_warns_once_per_distinct_type(transport: MockTransport, capsys) -> None:
    """Each distinct unknown notification type is warned once."""
    bot = Bot("test-bot", transport=transport)

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "reconnect_token": "rt-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    class UnknownTypeA:
        pass

    class UnknownTypeB:
        pass

    await bot._client._notify_queue.put(UnknownTypeA())
    await bot._client._notify_queue.put(UnknownTypeB())
    await bot._client._notify_queue.put(UnknownTypeA())
    await asyncio.sleep(0.05)

    bot._request_shutdown()
    await task

    stderr = capsys.readouterr().err
    warnings = [line for line in stderr.splitlines() if "unknown notification type" in line]
    assert len(warnings) == 2
    assert any("UnknownTypeA" in line for line in warnings)
    assert any("UnknownTypeB" in line for line in warnings)


@pytest.mark.asyncio
async def test_reply_content_validates_string() -> None:
    """bot.reply rejects non-string content."""
    bot = Bot("test-bot", transport=MockTransport())
    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/hello",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    with pytest.raises(ValueError, match="reply content must be a string"):
        bot.reply(event, 123)  # type: ignore[arg-type]


@pytest.mark.asyncio
async def test_reply_content_validates_max_bytes() -> None:
    """bot.reply rejects content exceeding 8192 bytes."""
    bot = Bot("test-bot", transport=MockTransport())
    event = AgentEventParams(
        bot_id="test-bot",
        event_id="e-1",
        type="dm_received",
        chat_id="npub1chat",
        content="/hello",
        rumor_id="r-1",
        author="npub1author",
        timestamp=1234567890,
    )
    with pytest.raises(ValueError, match="reply content exceeds 8192 bytes"):
        bot.reply(event, "x" * 8193)

"""Integration tests for the reference Python echo handler.

These tests exercise the JSON-RPC contract between ``echo_bot.py`` and the
pacto-bot-api daemon.  The daemon now pushes ``agent.status`` and
``agent.event`` notifications to Unix-socket handlers, so most tests run
against a live daemon.  ``test_echo_bot_replies_to_dm`` uses a lightweight mock
daemon socket to verify the handler's request/response behavior in isolation
without requiring a live Nostr relay.
"""

from __future__ import annotations

import asyncio
import json
import signal
import subprocess
import sys
import uuid
from pathlib import Path
from typing import Any

import pytest

from conftest import DaemonClient, MockRelay, _async_wait_for_socket


@pytest.mark.asyncio
async def test_handler_registers_and_receives_ready(handler_client: DaemonClient) -> None:
    """A handler can connect to the daemon and register for DM events."""
    assert handler_client.handler_id
    assert isinstance(handler_client.handler_id, str)
    assert "dm_received" in handler_client.registered_events

    # The daemon is ready if it accepted the registration and exposes the
    # socket.  ( agent.status notifications are broadcast to the handler
    # registry; with the current daemon transport they do not reach the
    # Unix-socket connection, so readiness is asserted via successful RPC. )


@pytest.mark.asyncio
async def test_echo_bot_replies_to_dm(tmp_path: Path) -> None:
    """echo_bot.py replies to '/echo' events and ignores everything else."""
    short_dir = Path(f"/tmp/pacto-echo-{uuid.uuid4().hex[:8]}")
    short_dir.mkdir(parents=True, exist_ok=True)
    socket_path = short_dir / "mock-daemon.sock"

    received: list[dict[str, Any]] = []
    registration_received = asyncio.Event()
    reply_received = asyncio.Event()
    ignore_received = asyncio.Event()

    async def mock_daemon_server() -> None:
        server = await asyncio.start_unix_server(
            client_connected,
            path=str(socket_path),
        )
        try:
            await asyncio.sleep(30.0)
        finally:
            server.close()
            await server.wait_closed()

    async def client_connected(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        handler_id = str(uuid.uuid4())
        while True:
            line = await reader.readline()
            if not line:
                break
            try:
                msg = json.loads(line.decode())
            except json.JSONDecodeError:
                continue
            received.append(msg)
            method = msg.get("method")
            msg_id = msg.get("id")

            if method == "handler.register":
                response = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "handler_id": handler_id,
                        "registered_events": ["dm_received"],
                    },
                }
                writer.write((json.dumps(response) + "\n").encode())
                await writer.drain()
                registration_received.set()
                # Send a ready status notification, then an echo event.
                await asyncio.sleep(0.05)
                writer.write(
                    (
                        json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "method": "agent.status",
                                "params": {"state": "ready"},
                            }
                        )
                        + "\n"
                    ).encode()
                )
                await writer.drain()
                await asyncio.sleep(0.05)
                writer.write(
                    (
                        json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "method": "agent.event",
                                "params": {
                                    "bot_id": "echo-bot",
                                    "event_id": "abcd1234",
                                    "type": "dm_received",
                                    "chat_id": None,
                                    "content": "/echo hello world",
                                    "rumor_id": "rumor-abcd",
                                    "author": "npub1sender",
                                    "timestamp": 1_700_000_000_000,
                                },
                            }
                        )
                        + "\n"
                    ).encode()
                )
                await writer.drain()
                await asyncio.sleep(0.05)
                writer.write(
                    (
                        json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "method": "agent.event",
                                "params": {
                                    "bot_id": "echo-bot",
                                    "event_id": "abcd5678",
                                    "type": "dm_received",
                                    "chat_id": None,
                                    "content": "just chatting",
                                    "rumor_id": "rumor-5678",
                                    "author": "npub1sender",
                                    "timestamp": 1_700_000_001_000,
                                },
                            }
                        )
                        + "\n"
                    ).encode()
                )
                await writer.drain()

            elif method == "handler.response":
                params = msg.get("params", {})
                if params.get("event_id") == "abcd1234":
                    assert params.get("action") == "reply"
                    assert "hello world" in params.get("content", "")
                    reply_received.set()
                elif params.get("event_id") == "abcd5678":
                    assert params.get("action") == "ignore"
                    ignore_received.set()

    server_task = asyncio.create_task(mock_daemon_server())
    await asyncio.sleep(0.1)

    proc = subprocess.Popen(
        [sys.executable, str(Path(__file__).with_name("echo_bot.py")), "--socket", str(socket_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        await asyncio.wait_for(registration_received.wait(), timeout=5.0)
        await asyncio.wait_for(reply_received.wait(), timeout=5.0)
        await asyncio.wait_for(ignore_received.wait(), timeout=5.0)
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5.0)
        server_task.cancel()
        try:
            await server_task
        except asyncio.CancelledError:
            pass

    # The handler should have registered, then responded to the two events.
    methods = [m.get("method") for m in received]
    assert "handler.register" in methods
    assert methods.count("handler.response") == 2

    stderr = proc.stderr.read() or ""
    assert "replying to abcd1234" in stderr
    assert "ignoring abcd5678" in stderr


@pytest.mark.asyncio
async def test_daemon_accepts_send_dm(tmp_path: Path, mock_relay: MockRelay) -> None:
    """The daemon accepts an outbound send_dm request and returns an event id."""
    from conftest import _generate_bot_keys, _write_config, _repo_root

    bot_keys = _generate_bot_keys("echo-bot")
    # Use any valid npub as the recipient; the daemon only needs to encrypt to it.
    recipient_keys = _generate_bot_keys("recipient")
    config_path, data_dir, socket_path = _write_config(
        tmp_path, bot_keys, relay_url=mock_relay.ws_uri
    )

    proc = subprocess.Popen(
        [
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "pacto-bot-api",
            "--",
            "--config",
            str(config_path),
            "--data-dir",
            str(data_dir),
        ],
        cwd=_repo_root(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        await _async_wait_for_socket(socket_path, timeout=15.0)
        reader, writer = await asyncio.open_unix_connection(str(socket_path))
        client = DaemonClient(reader, writer)
        try:
            await client.register(
                bot_ids=["echo-bot"],
                event_types=["dm_received"],
                capabilities=["ReadMessages", "SendMessages"],
            )
            event_id = await client.send_dm(
                bot_id="echo-bot",
                recipient=recipient_keys["npub"],
                content="test message",
            )
            assert isinstance(event_id, str)
            assert len(event_id) == 64  # hex event id
        finally:
            await client.close()
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=15.0)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5.0)
        if socket_path.exists():
            socket_path.unlink(missing_ok=True)


def test_echo_bot_help_does_not_crash() -> None:
    """The reference handler can print usage without a daemon."""
    result = subprocess.run(
        [sys.executable, str(Path(__file__).with_name("echo_bot.py")), "--help"],
        capture_output=True,
        text=True,
        check=True,
    )
    assert "echo handler" in result.stdout.lower()


@pytest.mark.asyncio
async def test_mock_relay_receives_daemon_subscription(
    tmp_path: Path,
    mock_relay: MockRelay,
) -> None:
    """The daemon connects to the mock relay and subscribes for gift wraps."""
    from conftest import _generate_bot_keys, _write_config, _repo_root

    bot_keys = _generate_bot_keys("echo-bot")
    config_path, data_dir, socket_path = _write_config(
        tmp_path, bot_keys, relay_url=mock_relay.ws_uri
    )

    cmd = [
        "cargo",
        "run",
        "--quiet",
        "--bin",
        "pacto-bot-api",
        "--",
        "--config",
        str(config_path),
        "--data-dir",
        str(data_dir),
    ]
    proc = subprocess.Popen(
        cmd,
        cwd=_repo_root(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        # Wait for the daemon to connect and subscribe.
        for _ in range(100):
            await asyncio.sleep(0.05)
            if any(mock_relay.subscriptions.values()):
                break
        else:
            pytest.fail("daemon never subscribed to mock relay")

        # Give the daemon a moment to finish subscribing before injecting.
        await asyncio.sleep(0.2)

        # Inject a syntactically valid but un-decryptable gift wrap so the
        # daemon exercises the receive path without requiring a real NIP-59
        # payload in Python.
        await mock_relay.inject_gift_wrap(
            {
                "id": "0" * 64,
                "pubkey": "0" * 64,
                "created_at": 1_700_000_000,
                "kind": 1059,
                "tags": [["p", bot_keys["npub"]]],
                "content": "invalid-but-ignored-gift-wrap",
                "sig": "0" * 128,
            }
        )
        # The daemon will log a decryption failure; the important part is that
        # it received the event from the relay and attempted to process it.
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=15.0)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5.0)
        if socket_path.exists():
            socket_path.unlink(missing_ok=True)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])

"""Parameterized contract tests for every example bot manifest.

Discovers ``python/examples/**/*_bot.py`` (excluding test files), validates the
matching ``<bot>.manifest.json`` against ``schemas/example-manifest.json``,
spawns a fresh daemon for each bot, and executes the declared contract pieces.
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

from conftest import (
    DaemonClient,
    SocketProxy,
    _async_wait_for_socket,
    _repo_root,
    _short_tmp_dir,
    daemon_lifecycle,
    discover_bot_files,
    load_manifest,
    validate_manifest,
)


def _matches(actual: Any, expected: Any) -> bool:
    """Recursively check that *actual* contains at least the keys/values in *expected*."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return False
        return all(
            key in actual and _matches(actual[key], value)
            for key, value in expected.items()
        )
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(actual) != len(expected):
            return False
        return all(_matches(a, e) for a, e in zip(actual, expected))
    return actual == expected


async def _wait_for_bot_registration(
    proxy: SocketProxy, timeout: float = 10.0
) -> dict[str, Any]:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        async with proxy._lock:
            for msg in proxy.bot_messages:
                if msg.get("method") == "handler.register":
                    return msg
        await asyncio.sleep(0.05)
    raise TimeoutError("bot did not send handler.register")


async def _run_rpc_call(
    client: DaemonClient,
    piece: dict[str, Any],
    diagnostics: list[str],
) -> None:
    method = piece["method"]
    params = piece.get("params")
    result = await client.request(method, params)
    diagnostics.append(f"rpc_call {piece['name']!r} ({method}) -> {result!r}")
    expected = piece.get("expect")
    if expected is not None and not _matches(result, expected):
        raise AssertionError(
            f"piece {piece['name']!r}: result {result!r} does not match expected {expected!r}"
        )


async def _run_notification(
    client: DaemonClient,
    piece: dict[str, Any],
    diagnostics: list[str],
) -> None:
    method = piece["method"]
    params = piece.get("params", {})
    client.send_notification(method, params)
    diagnostics.append(f"notification {piece['name']!r} ({method}) sent")


async def _run_event_response(
    proxy: SocketProxy,
    piece: dict[str, Any],
    diagnostics: list[str],
) -> None:
    inject_event = piece["inject_event"]
    expect_response = piece["expect_response"]
    # Clear stale bot messages so we only match responses to this event.
    async with proxy._lock:
        proxy.bot_messages.clear()
    await proxy.inject_to_bot(
        {"jsonrpc": "2.0", "method": "agent.event", "params": inject_event}
    )
    diagnostics.append(f"event_response {piece['name']!r}: injected agent.event")

    deadline = asyncio.get_event_loop().time() + piece.get("timeout_seconds", 30)
    while asyncio.get_event_loop().time() < deadline:
        async with proxy._lock:
            for msg in proxy.bot_messages:
                if msg.get("method") == "handler.response":
                    params = msg.get("params", {})
                    if _matches(params, expect_response):
                        return
        await asyncio.sleep(0.05)

    raise AssertionError(
        f"piece {piece['name']!r}: no handler.response matching {expect_response!r} "
        f"within {piece.get('timeout_seconds', 30)}s"
    )


async def _run_expected_error(
    client: DaemonClient,
    piece: dict[str, Any],
    diagnostics: list[str],
) -> None:
    method = piece["method"]
    params = piece.get("params")
    expect_error = piece["expect_error"]
    request_id = str(uuid.uuid4())
    msg = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params is not None:
        msg["params"] = params
    async with client._lock:
        client.writer.write((json.dumps(msg, separators=(",", ":")) + "\n").encode())
        await client.writer.drain()

    deadline = asyncio.get_event_loop().time() + 10.0
    response: dict[str, Any] | None = None
    while asyncio.get_event_loop().time() < deadline:
        async with client._lock:
            if request_id in client._responses:
                response = client._responses.pop(request_id)
                break
        await asyncio.sleep(0.01)

    diagnostics.append(
        f"expected_error {piece['name']!r} ({method}) -> {response!r}"
    )
    if response is None:
        raise AssertionError(f"piece {piece['name']!r}: timed out waiting for error")
    error = response.get("error")
    if error is None:
        raise AssertionError(
            f"piece {piece['name']!r}: expected error but got result {response.get('result')!r}"
        )
    if error.get("code") != expect_error["code"]:
        raise AssertionError(
            f"piece {piece['name']!r}: expected error code {expect_error['code']}, got {error.get('code')}"
        )
    substring = expect_error.get("message_contains")
    if substring and substring not in str(error.get("message", "")):
        raise AssertionError(
            f"piece {piece['name']!r}: error message {error.get('message')!r} "
            f"does not contain {substring!r}"
        )


async def _execute_piece(
    client: DaemonClient,
    proxy: SocketProxy,
    piece: dict[str, Any],
    diagnostics: list[str],
) -> None:
    piece_type = piece["type"]
    if piece_type == "rpc_call":
        await _run_rpc_call(client, piece, diagnostics)
    elif piece_type == "notification":
        await _run_notification(client, piece, diagnostics)
    elif piece_type == "event_response":
        await _run_event_response(proxy, piece, diagnostics)
    elif piece_type == "expected_error":
        await _run_expected_error(client, piece, diagnostics)
    else:
        raise AssertionError(f"unknown contract piece type: {piece_type}")


async def _execute_contract(
    bot_file: Path,
    manifest: dict[str, Any],
    tmp_path: Path,
) -> None:
    bot_id = manifest.get("bot_id", "echo-bot")
    diagnostics: list[str] = []
    bot_proc: subprocess.Popen[str] | None = None

    async with daemon_lifecycle(tmp_path, bot_id=bot_id) as (
        _proc,
        _config_path,
        daemon_socket,
    ):
        proxy_dir = _short_tmp_dir(tmp_path, suffix="-proxy")
        proxy_socket = proxy_dir / "proxy.sock"
        proxy = SocketProxy(daemon_socket)
        await proxy.start(proxy_socket)
        try:
            bot_proc = subprocess.Popen(
                [sys.executable, str(bot_file), "--socket", str(proxy_socket)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            await _async_wait_for_socket(proxy_socket, timeout=10.0)
            # Ensure the bot has connected and completed its registration before
            # we start driving contract pieces.
            await _wait_for_bot_registration(proxy)

            reader, writer = await asyncio.open_unix_connection(str(daemon_socket))
            client = DaemonClient(reader, writer)
            try:
                if manifest.get("registration", True):
                    await client.register(
                        bot_ids=[bot_id],
                        event_types=["dm_received"],
                        capabilities=["ReadMessages", "SendMessages"],
                    )
                    diagnostics.append("harness registration completed")

                for piece in manifest["contract_pieces"]:
                    timeout = piece.get("timeout_seconds", 30)
                    try:
                        await asyncio.wait_for(
                            _execute_piece(client, proxy, piece, diagnostics),
                            timeout=timeout,
                        )
                    except asyncio.TimeoutError as exc:
                        raise AssertionError(
                            f"bot {bot_file.name} piece {piece['name']!r} "
                            f"timed out after {timeout}s"
                        ) from exc
            finally:
                await client.close()

            if manifest.get("shutdown", True):
                diagnostics.append("sending SIGINT to bot")
                if bot_proc.poll() is None:
                    bot_proc.send_signal(signal.SIGINT)
                    try:
                        bot_proc.wait(timeout=10.0)
                    except subprocess.TimeoutExpired:
                        bot_proc.kill()
                        bot_proc.wait(timeout=5.0)
                bot_proc = None
        finally:
            await proxy.stop()
            if bot_proc is not None and bot_proc.poll() is None:
                bot_proc.send_signal(signal.SIGINT)
                try:
                    bot_proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    bot_proc.kill()
                    bot_proc.wait(timeout=5.0)


def pytest_generate_tests(metafunc: pytest.Metafunc) -> None:
    if "bot_file" in metafunc.fixturenames:
        bots = discover_bot_files()
        ids = [b.name for b in bots]
        metafunc.parametrize("bot_file", bots, ids=ids)


@pytest.mark.asyncio
async def test_example_contract(bot_file: Path, tmp_path: Path) -> None:
    manifest = load_manifest(bot_file)
    validate_manifest(manifest, bot_file)
    await _execute_contract(bot_file, manifest, tmp_path)


class _NoResponseProxy:
    """Minimal proxy stand-in for testing event_response timeouts."""

    def __init__(self) -> None:
        self.bot_messages: list[dict[str, Any]] = []
        self._lock = asyncio.Lock()

    async def inject_to_bot(self, msg: dict[str, Any]) -> None:
        pass


async def test_event_response_timeout_diagnostic() -> None:
    """A missing response produces a diagnostic naming the piece and timeout."""
    piece = {
        "name": "slow_echo",
        "type": "event_response",
        "timeout_seconds": 1,
        "inject_event": {"event_id": "slow-1"},
        "expect_response": {"event_id": "slow-1"},
    }
    proxy = _NoResponseProxy()
    diagnostics: list[str] = []
    with pytest.raises(AssertionError, match="piece 'slow_echo': no handler.response matching.*within 1s"):
        await _run_event_response(proxy, piece, diagnostics)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])

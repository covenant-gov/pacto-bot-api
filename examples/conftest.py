"""pytest fixtures for pacto-bot-api example tests.

These fixtures spin up the Rust daemon, connect a JSON-RPC handler, and provide
a minimal WebSocket Nostr relay for end-to-end DM tests.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import signal
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any, AsyncGenerator, Callable

import pytest
import websockets
import websockets.server


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def _daemon_bin() -> str:
    env = os.environ.get("CARGO_BIN_EXE_pacto-bot-api")
    if env:
        return env
    binary = _repo_root() / "target" / "debug" / "pacto-bot-api"
    if binary.exists():
        return str(binary)
    # Fall back to cargo run (slower but always works in a source checkout).
    return "cargo"


def _generate_bot_keys(bot_id: str = "echo-bot") -> dict[str, str]:
    """Generate an npub/nsec pair using the Rust admin CLI."""
    cmd = [
        "cargo",
        "run",
        "--quiet",
        "--bin",
        "pacto-bot-admin",
        "--",
        "new",
        bot_id,
        "--backend",
        "nsec",
    ]
    result = subprocess.run(
        cmd,
        cwd=_repo_root(),
        capture_output=True,
        text=True,
        check=True,
        timeout=120,
    )
    output = result.stdout + result.stderr
    npub_match = re.search(r'npub\s*=\s*"([^"]+)"', output)
    nsec_match = re.search(r'nsec\s*=\s*"([^"]+)"', output)
    if not npub_match or not nsec_match:
        raise RuntimeError(f"failed to parse admin CLI output:\n{output}")
    return {"npub": npub_match.group(1), "nsec": nsec_match.group(1)}


def _short_tmp_dir(tmp_path: Path, suffix: str = "") -> Path:
    """Return a short path usable for Unix sockets.

    macOS temp paths can exceed the 104-byte AF_UNIX limit, so we create a
    short directory under /tmp and symlink it from the pytest tmp_path.
    """
    short = Path(f"/tmp/pacto{suffix}-{uuid.uuid4().hex[:8]}")
    short.mkdir(parents=True, exist_ok=True)
    link = tmp_path / "short"
    if not link.exists():
        link.symlink_to(short)
    return short


def _write_config(
    tmp_path: Path,
    bot_keys: dict[str, str],
    relay_url: str = "ws://127.0.0.1:5555",
) -> tuple[Path, Path, Path]:
    """Write a daemon config and return (config_path, data_dir, socket_path)."""
    short_dir = _short_tmp_dir(tmp_path)
    config_path = short_dir / "pacto-bot-api.toml"
    data_dir = short_dir / "data"
    data_dir.mkdir(parents=True, exist_ok=True)
    socket_path = data_dir / "pacto-bot-api.sock"
    config = f"""[daemon]
data_dir = {str(data_dir)!r}
socket_path = {str(socket_path)!r}

[[bots]]
id = "echo-bot"
npub = "{bot_keys['npub']}"
signing = {{ backend = "nsec", nsec = "{bot_keys['nsec']}" }}
relays = [{relay_url!r}]
capabilities = ["ReadMessages", "SendMessages"]
"""
    config_path.write_text(config)
    if sys.platform != "win32":
        config_path.chmod(0o600)
    return config_path, data_dir, socket_path


def _wait_for_socket(socket_path: Path, timeout: float = 15.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if socket_path.exists():
            # On some platforms the file may exist before the listener is ready;
            # try a quick connect to be sure.
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                    sock.settimeout(0.1)
                    sock.connect(str(socket_path))
                    return
            except (OSError, ConnectionRefusedError):
                pass
        time.sleep(0.05)
    raise TimeoutError(f"daemon socket did not appear: {socket_path}")


async def _async_wait_for_socket(socket_path: Path, timeout: float = 15.0) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if socket_path.exists():
            try:
                r, w = await asyncio.wait_for(
                    asyncio.open_unix_connection(str(socket_path)), timeout=0.2
                )
                r.feed_eof()
                w.close()
                await w.wait_closed()
                return
            except (OSError, asyncio.TimeoutError):
                pass
        await asyncio.sleep(0.05)
    raise TimeoutError(f"daemon socket did not appear: {socket_path}")


# ---------------------------------------------------------------------------
# DaemonClient
# ---------------------------------------------------------------------------


class DaemonClient:
    """Connected JSON-RPC client for the pacto-bot-api daemon."""

    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self.reader = reader
        self.writer = writer
        self.handler_id: str | None = None
        self.registered_events: list[str] = []
        self._lock = asyncio.Lock()
        self._events: list[dict[str, Any]] = []
        self._responses: dict[str, dict[str, Any]] = {}
        self._read_task = asyncio.create_task(self._read_loop())
        self._closed = False

    async def register(
        self,
        bot_ids: list[str],
        event_types: list[str],
        capabilities: list[str],
    ) -> dict[str, Any]:
        result = await self.request(
            "handler.register",
            {
                "bot_ids": bot_ids,
                "event_types": event_types,
                "capabilities": capabilities,
            },
        )
        self.handler_id = result.get("handler_id")
        self.registered_events = result.get("registered_events", [])
        return result

    async def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        request_id = str(uuid.uuid4())
        msg = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            msg["params"] = params
        async with self._lock:
            self.writer.write((json.dumps(msg, separators=(",", ":")) + "\n").encode())
            await self.writer.drain()
        deadline = asyncio.get_event_loop().time() + 10.0
        while asyncio.get_event_loop().time() < deadline:
            async with self._lock:
                if request_id in self._responses:
                    return self._responses.pop(request_id)
            await asyncio.sleep(0.01)
        raise TimeoutError(f"no response for {method}")

    def send_notification(self, method: str, params: dict[str, Any]) -> None:
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        self.writer.write((json.dumps(msg, separators=(",", ":")) + "\n").encode())

    async def send_dm(
        self,
        bot_id: str,
        recipient: str,
        content: str,
        reply_to: str | None = None,
    ) -> str:
        params: dict[str, Any] = {
            "bot_id": bot_id,
            "recipient": recipient,
            "content": content,
        }
        if reply_to is not None:
            params["reply_to"] = reply_to
        result = await self.request("agent.send_dm", params)
        # Result is the published event id as a hex string.
        return str(result)

    async def wait_for_event(
        self,
        bot_id: str = "echo-bot",
        event_type: str = "dm_received",
        timeout: float = 5.0,
    ) -> dict[str, Any]:
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            async with self._lock:
                for ev in self._events:
                    if ev.get("bot_id") == bot_id and ev.get("type") == event_type:
                        self._events.remove(ev)
                        return ev
            await asyncio.sleep(0.05)
        raise TimeoutError(f"no {event_type} event for {bot_id}")

    async def assert_reply_contains(
        self,
        bot_id: str,
        text: str,
        timeout: float = 5.0,
    ) -> None:
        """Wait until a DM reply containing *text* is observed.

        The daemon pushes ``agent.event`` notifications to Unix-socket handlers,
        so this helper waits for those events and matches on the reply content.
        It is used by tests that drive the real daemon end-to-end; the mock-daemon
        test ``test_echo_bot_replies_to_dm`` exercises the same contract in
        isolation.
        """
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            async with self._lock:
                for ev in list(self._events):
                    if ev.get("bot_id") == bot_id and text in ev.get("content", ""):
                        self._events.remove(ev)
                        return
            await asyncio.sleep(0.05)
        raise AssertionError(f"no reply containing {text!r} for {bot_id}")

    async def _read_loop(self) -> None:
        try:
            while True:
                line = await self.reader.readline()
                if not line:
                    break
                try:
                    msg = json.loads(line.decode())
                except json.JSONDecodeError:
                    continue
                method = msg.get("method")
                if method == "agent.event":
                    async with self._lock:
                        self._events.append(msg.get("params", {}))
                elif "id" in msg:
                    async with self._lock:
                        self._responses[str(msg["id"])] = msg.get("result", msg)
        except asyncio.CancelledError:
            pass

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._read_task.cancel()
        try:
            await self._read_task
        except asyncio.CancelledError:
            pass
        self.writer.close()
        await self.writer.wait_closed()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
async def daemon(
    tmp_path: Path,
) -> AsyncGenerator[tuple[subprocess.Popen[bytes], Path], None]:
    """Spawn the Rust daemon and yield once its Unix socket is ready."""
    bot_keys = _generate_bot_keys("echo-bot")
    config_path, data_dir, socket_path = _write_config(tmp_path, bot_keys)

    cmd: list[str]
    bin_arg = _daemon_bin()
    if bin_arg == "cargo":
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
    else:
        cmd = [
            bin_arg,
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
        await _async_wait_for_socket(socket_path, timeout=20.0)
        yield proc, socket_path
    except Exception as exc:
        # Surface daemon stderr to make startup failures debuggable.
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
        stdout, stderr = proc.communicate(timeout=10.0)
        raise RuntimeError(
            f"daemon failed to start: {exc}\nSTDOUT:\n{stdout.decode(errors='replace')}\n"
            f"STDERR:\n{stderr.decode(errors='replace')}"
        ) from exc
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=20.0)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5.0)
        # Clean up the socket so later tests can re-bind the same path.
        if socket_path.exists():
            socket_path.unlink(missing_ok=True)


@pytest.fixture
async def handler_client(
    daemon: tuple[subprocess.Popen[bytes], Path],
) -> AsyncGenerator[DaemonClient, None]:
    """Connect to the daemon and register a test handler."""
    proc, socket_path = daemon
    await _async_wait_for_socket(socket_path, timeout=10.0)
    reader, writer = await asyncio.open_unix_connection(str(socket_path))
    client = DaemonClient(reader, writer)
    try:
        await client.register(
            bot_ids=["echo-bot"],
            event_types=["dm_received"],
            capabilities=["ReadMessages", "SendMessages"],
        )
        yield client
    finally:
        await client.close()


# ---------------------------------------------------------------------------
# Mock relay
# ---------------------------------------------------------------------------


class MockRelay:
    """Minimal Nostr relay for local end-to-end tests."""

    def __init__(self, ws_uri: str) -> None:
        self.ws_uri = ws_uri
        self.subscriptions: dict[str, list[str]] = {}
        self.events: list[dict[str, Any]] = []
        self.clients: set[websockets.server.WebSocketServerProtocol] = set()
        self.server: websockets.server.WebSocketServer | None = None

    async def inject_gift_wrap(self, event: dict[str, Any]) -> None:
        """Push a kind-1059 gift-wrap event to all active subscriptions."""
        self.events.append(event)
        for ws in list(self.clients):
            if ws.state.name != "OPEN":
                continue
            for sub_id in self.subscriptions.get(str(ws.id), []):
                try:
                    await ws.send(json.dumps(["EVENT", sub_id, event]))
                except Exception:
                    pass

    async def handler(self, ws: websockets.server.WebSocketServerProtocol) -> None:
        client_id = str(ws.id)
        self.clients.add(ws)
        self.subscriptions[client_id] = []
        try:
            async for raw in ws:
                try:
                    msg = json.loads(raw)
                except json.JSONDecodeError:
                    await ws.send(json.dumps(["NOTICE", "invalid: json"]))
                    continue
                if not isinstance(msg, list) or not msg:
                    continue
                verb = msg[0]
                if verb == "REQ" and len(msg) >= 2:
                    sub_id = msg[1]
                    self.subscriptions[client_id].append(sub_id)
                    # Replay any previously injected events.
                    for ev in self.events:
                        await ws.send(json.dumps(["EVENT", sub_id, ev]))
                    await ws.send(json.dumps(["EOSE", sub_id]))
                elif verb == "CLOSE" and len(msg) >= 2:
                    sub_id = msg[1]
                    self.subscriptions[client_id] = [
                        s for s in self.subscriptions[client_id] if s != sub_id
                    ]
                elif verb == "EVENT":
                    # Daemon publishes here; record it and acknowledge.
                    event = msg[1]
                    self.events.append(event)
                    ok = json.dumps(["OK", event.get("id", ""), True, ""])
                    await ws.send(ok)
        finally:
            self.clients.discard(ws)
            self.subscriptions.pop(client_id, None)


@pytest.fixture
async def mock_relay(tmp_path: Path) -> AsyncGenerator[MockRelay, None]:
    """Start a minimal WebSocket relay on an ephemeral port."""
    import socket as stdlib_socket

    sock = stdlib_socket.socket(stdlib_socket.AF_INET, stdlib_socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()

    relay = MockRelay(f"ws://127.0.0.1:{port}")
    stop = asyncio.Future()

    async def _serve() -> None:
        relay.server = await websockets.serve(
            relay.handler,
            "127.0.0.1",
            port,
        )
        try:
            await stop
        finally:
            relay.server.close()
            await relay.server.wait_closed()

    task = asyncio.create_task(_serve())
    # Give the server a moment to start listening.
    await asyncio.sleep(0.1)
    try:
        yield relay
    finally:
        stop.set_result(None)
        await task

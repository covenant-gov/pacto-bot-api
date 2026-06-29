"""Tests for the pacto-bot-api transport adapters."""

from __future__ import annotations

import asyncio
import json
from typing import Any
from unittest.mock import AsyncMock, MagicMock

import pytest

from pacto_bot_api.transports import (
    HttpTransport,
    Transport,
    UnixTransport,
    _resolve_http_bind,
    _resolve_socket_path,
    resolve_http_secret,
)


# ---------------------------------------------------------------------------
# UnixTransport
# ---------------------------------------------------------------------------


class FakeStreamReader:
    """Asyncio StreamReader stand-in for tests."""

    def __init__(self, lines: list[str]):
        self._lines = list(lines)
        self._index = 0

    async def readline(self) -> bytes:
        if self._index >= len(self._lines):
            return b""
        line = self._lines[self._index]
        self._index += 1
        return line.encode("utf-8")


class FakeStreamWriter:
    """Asyncio StreamWriter stand-in for tests."""

    def __init__(self):
        self.data: list[bytes] = []
        self.closed = False

    def write(self, data: bytes) -> None:
        self.data.append(data)

    async def drain(self) -> None:
        pass

    def close(self) -> None:
        self.closed = True

    async def wait_closed(self) -> None:
        pass


@pytest.mark.asyncio
async def test_unix_transport_connect_and_roundtrip(monkeypatch):
    reader = FakeStreamReader(['{"method":"agent.status","params":{"state":"ok"}}\n'])
    writer = FakeStreamWriter()

    async def fake_open_unix_connection(path: str):
        assert path == "/tmp/pacto.sock"
        return reader, writer

    monkeypatch.setattr(
        asyncio, "open_unix_connection", fake_open_unix_connection
    )

    transport = UnixTransport("/tmp/pacto.sock")
    await transport.connect()

    line = await transport.readline()
    assert json.loads(line) == {
        "method": "agent.status",
        "params": {"state": "ok"},
    }

    await transport.write_frame({"jsonrpc": "2.0", "method": "agent.send_dm"})
    written = b"".join(writer.data).decode("utf-8")
    assert json.loads(written) == {
        "jsonrpc": "2.0",
        "method": "agent.send_dm",
    }

    await transport.close()
    assert writer.closed


@pytest.mark.asyncio
async def test_unix_transport_readline_when_disconnected():
    transport = UnixTransport("/tmp/pacto.sock")
    with pytest.raises(RuntimeError, match="transport not connected"):
        await transport.readline()


# ---------------------------------------------------------------------------
# HttpTransport
# ---------------------------------------------------------------------------


class FakeHttpPair:
    """Holds a fake reader/writer pair and the request bytes sent to it."""

    def __init__(self, lines: list[bytes] | None = None):
        self.request_data: list[bytes] = []
        self._lines = list(lines or [])
        self.closed = False

    def reader(self) -> "FakeHttpReader":
        return FakeHttpReader(self._lines)

    def writer(self) -> "FakeHttpWriter":
        return FakeHttpWriter(self)


class FakeHttpReader:
    def __init__(self, lines: list[bytes]):
        self._lines = list(lines)
        self._index = 0

    async def readline(self) -> bytes:
        if self._index >= len(self._lines):
            return b""
        line = self._lines[self._index]
        self._index += 1
        return line

    async def readexactly(self, n: int) -> bytes:
        remaining = b"".join(self._lines[self._index :])
        self._index = len(self._lines)
        return remaining[:n]

    async def read(self) -> bytes:
        remaining = b"".join(self._lines[self._index :])
        self._index = len(self._lines)
        return remaining


class FakeHttpWriter:
    def __init__(self, pair: FakeHttpPair):
        self._pair = pair

    def write(self, data: bytes) -> None:
        self._pair.request_data.append(data)

    async def drain(self) -> None:
        pass

    def close(self) -> None:
        self._pair.closed = True

    async def wait_closed(self) -> None:
        pass


def _http_lines(body_lines: list[bytes]) -> list[bytes]:
    return [
        b"HTTP/1.1 200 OK\r\n",
        b"\r\n",
        *body_lines,
    ]


@pytest.fixture
def mock_http_connection(monkeypatch):
    """Return a factory that patches asyncio.open_connection with a fake pair."""
    pairs: list[FakeHttpPair] = []

    def factory(lines: list[bytes] | None = None):
        pair = FakeHttpPair(lines or [])
        pairs.append(pair)

        async def fake_open_connection(host: str, port: int):
            assert host == "127.0.0.1"
            assert port == 9800
            return pair.reader(), pair.writer()

        monkeypatch.setattr(asyncio, "open_connection", fake_open_connection)
        return pair

    return factory


@pytest.mark.asyncio
async def test_http_transport_write_frame_sends_secret_and_handler_id(mock_http_connection):
    pair = mock_http_connection(_http_lines([b'']))
    transport = HttpTransport("127.0.0.1", 9800, "super-secret", handler_id="h-1")
    await transport.connect()

    await transport.write_frame(
        {"jsonrpc": "2.0", "method": "agent.send_dm", "params": {}}
    )

    request = b"".join(pair.request_data).decode("utf-8")
    assert "POST / HTTP/1.1" in request
    assert "X-Pacto-Bot-Secret: super-secret" in request
    assert "X-Pacto-Handler-Id: h-1" in request
    assert "Content-Type: application/json" in request


@pytest.mark.asyncio
async def test_http_transport_write_frame_omits_handler_id_for_non_mutating(mock_http_connection):
    pair = mock_http_connection(_http_lines([b""]))
    transport = HttpTransport("127.0.0.1", 9800, "super-secret", handler_id="h-1")
    await transport.connect()

    await transport.write_frame(
        {"jsonrpc": "2.0", "method": "handler.register", "params": {}}
    )

    request = b"".join(pair.request_data).decode("utf-8")
    assert "X-Pacto-Bot-Secret: super-secret" in request
    assert "X-Pacto-Handler-Id" not in request


@pytest.mark.asyncio
async def test_http_transport_write_frame_reads_inline_response(mock_http_connection):
    response = json.dumps({"jsonrpc": "2.0", "id": "req-1", "result": "ok"})
    pair = mock_http_connection(_http_lines([response.encode("utf-8")]))
    transport = HttpTransport("127.0.0.1", 9800, "secret")
    await transport.connect()

    result = await transport.write_frame(
        {"jsonrpc": "2.0", "id": "req-1", "method": "agent.send_dm"}
    )

    assert result == {"jsonrpc": "2.0", "id": "req-1", "result": "ok"}


@pytest.mark.asyncio
async def test_http_transport_sse_parses_data_lines(mock_http_connection):
    sse_lines = [
        b"event: dm\r\n",
        b'data: {"method":"agent.event","params":{}}\r\n',
        b"\r\n",
        b'data: {"method":"agent.status","params":{"state":"ok"}}\r\n',
        b"\r\n",
    ]
    pair = mock_http_connection(_http_lines(sse_lines))
    transport = HttpTransport("127.0.0.1", 9800, "secret", handler_id="h-1")
    await transport.connect()
    await transport.start_sse()

    line1 = await transport.readline()
    line2 = await transport.readline()

    assert json.loads(line1) == {"method": "agent.event", "params": {}}
    assert json.loads(line2) == {
        "method": "agent.status",
        "params": {"state": "ok"},
    }


@pytest.mark.asyncio
async def test_http_transport_missing_secret_raises_at_connect():
    transport = HttpTransport("127.0.0.1", 9800, "")
    with pytest.raises(RuntimeError, match="secret token"):
        await transport.connect()


# ---------------------------------------------------------------------------
# Resolution helpers
# ---------------------------------------------------------------------------


def test_resolve_socket_path_explicit():
    assert _resolve_socket_path("/custom.sock", None) == "/custom.sock"


def test_resolve_socket_path_defaults():
    path = _resolve_socket_path(None, None)
    assert path.endswith("pacto-bot-api.sock")


def test_resolve_http_bind_defaults():
    assert _resolve_http_bind(None) == ("127.0.0.1", 9800)


def test_resolve_http_bind_custom():
    assert _resolve_http_bind("0.0.0.0:8080") == ("0.0.0.0", 8080)


def test_resolve_http_secret_from_arg():
    assert resolve_http_secret("arg-secret", "/tmp") == "arg-secret"


def test_resolve_http_secret_missing(tmp_path, monkeypatch):
    monkeypatch.delenv("PACTO_SECRET_TOKEN", raising=False)
    with pytest.raises(RuntimeError, match="secret token"):
        resolve_http_secret(None, str(tmp_path))


def test_resolve_http_secret_from_file(tmp_path, monkeypatch):
    monkeypatch.delenv("PACTO_SECRET_TOKEN", raising=False)
    token_path = tmp_path / "bot_secret_token"
    token_path.write_text("file-secret\n")
    assert resolve_http_secret(None, str(tmp_path)) == "file-secret"


# ---------------------------------------------------------------------------
# Protocol
# ---------------------------------------------------------------------------


def test_transport_protocol_is_runtime_checkable():
    assert isinstance(UnixTransport("/tmp/x.sock"), Transport)

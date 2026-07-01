"""Transport adapters for the pacto-bot-api Python SDK.

Provides a Unix-socket (NDJSON) transport and an HTTP+SSE transport using
only the Python standard library asyncio.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, Protocol, runtime_checkable


# ---------------------------------------------------------------------------
# Connection / path resolution helpers
# ---------------------------------------------------------------------------


def _default_data_dir() -> str:
    return str(Path.home() / ".local" / "share" / "pacto-bot-api")


def _resolve_data_dir(data_dir: str | None) -> str:
    return data_dir or os.environ.get("PACTO_DATA_DIR") or _default_data_dir()


def _resolve_socket_path(socket_path: str | None, data_dir: str | None) -> str:
    if socket_path:
        return socket_path
    socket_path = os.environ.get("PACTO_SOCKET", "")
    if socket_path:
        return socket_path
    data_dir = data_dir or os.environ.get("PACTO_DATA_DIR", "")
    if data_dir:
        return str(Path(data_dir) / "pacto-bot-api.sock")
    return str(Path(_default_data_dir()) / "pacto-bot-api.sock")


def _resolve_http_bind(http_bind: str | None) -> tuple[str, int]:
    value = http_bind or os.environ.get("PACTO_HTTP_BIND") or "127.0.0.1:9800"
    host, _, port_str = value.rpartition(":")
    if not host:
        host = "127.0.0.1"
    return host, int(port_str)


def resolve_http_secret(secret: str | None, data_dir: str | None = None) -> str:
    """Resolve the HTTP transport secret.

    Precedence: explicit ``secret`` → ``$PACTO_SECRET_TOKEN`` →
    ``<data_dir>/bot_secret_token``. Raises a clear error if none is found.
    """
    if secret:
        return secret
    secret = os.environ.get("PACTO_SECRET_TOKEN", "")
    if secret:
        return secret
    resolved_data_dir = _resolve_data_dir(data_dir)
    token_path = Path(resolved_data_dir) / "bot_secret_token"
    if token_path.exists():
        return token_path.read_text().strip()
    raise RuntimeError(
        "HTTP transport requires a secret token via --secret, "
        "$PACTO_SECRET_TOKEN, or <data_dir>/bot_secret_token"
    )


# ---------------------------------------------------------------------------
# Transport Protocol
# ---------------------------------------------------------------------------


@runtime_checkable
class Transport(Protocol):
    """Common interface for SDK transports."""

    @property
    def name(self) -> str:
        """Human-readable transport identifier."""
        ...

    async def connect(self) -> None:
        """Open the transport connection."""
        ...

    async def readline(self) -> str:
        """Read one NDJSON line or SSE ``data:`` payload."""
        ...

    async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
        """Write one JSON-RPC frame and optionally return the response inline."""
        ...

    async def close(self) -> None:
        """Close the transport connection."""
        ...


# ---------------------------------------------------------------------------
# Unix socket transport
# ---------------------------------------------------------------------------


class UnixTransport:
    """NDJSON-over-Unix-socket transport."""

    def __init__(self, socket_path: str):
        self.socket_path = socket_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None

    @property
    def name(self) -> str:
        return f"unix:{self.socket_path}"

    async def connect(self) -> None:
        socket_path = Path(self.socket_path)
        if not socket_path.exists():
            raise ConnectionError(
                "Cannot connect to pacto-bot-api daemon: "
                f"Unix socket not found at {self.socket_path}.\n"
                "Is the daemon running? If you are using the `bot-only` Docker profile, "
                "start the daemon on the host first, or switch to the `full` profile."
            )
        try:
            self._reader, self._writer = await asyncio.open_unix_connection(
                self.socket_path
            )
        except OSError as exc:
            raise ConnectionError(
                "Cannot connect to pacto-bot-api daemon: "
                f"failed to open Unix socket at {self.socket_path}: {exc}.\n"
                "Is the daemon running? If you are using the `bot-only` Docker profile, "
                "start the daemon on the host first, or switch to the `full` profile."
            ) from exc

    async def readline(self) -> str:
        if self._reader is None:
            raise RuntimeError("transport not connected")
        line = await self._reader.readline()
        if not line:
            return ""
        return line.decode("utf-8").strip()

    async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
        if self._writer is None:
            raise RuntimeError("transport not connected")
        line = json.dumps(frame, separators=(",", ":")) + "\n"
        self._writer.write(line.encode("utf-8"))
        await self._writer.drain()
        return None

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            await self._writer.wait_closed()
        self._reader = None
        self._writer = None


# ---------------------------------------------------------------------------
# HTTP+SSE transport
# ---------------------------------------------------------------------------


class HttpTransport:
    """HTTP+SSE localhost transport using plain asyncio TCP streams.

    Outbound frames are sent as ``POST /`` with ``X-Pacto-Bot-Secret``.
    Mutating methods (``agent.send_dm``, ``agent.set_profile``,
    ``agent.error``) also include ``X-Pacto-Handler-Id``. Inbound daemon
    notifications are consumed from ``GET /events?handler_id=<id>`` as a
    text/event-stream.
    """

    MUTATING_METHODS = {"agent.send_dm", "agent.set_profile", "agent.error"}

    def __init__(
        self,
        host: str,
        port: int,
        secret: str,
        handler_id: str | None = None,
    ):
        self.host = host
        self.port = port
        self.secret = secret
        self.handler_id = handler_id
        self._sse_reader: asyncio.StreamReader | None = None
        self._sse_writer: asyncio.StreamWriter | None = None
        self._closed = False

    @property
    def name(self) -> str:
        return f"http://{self.host}:{self.port}"

    async def connect(self) -> None:
        if not self.secret:
            raise RuntimeError("HTTP transport requires a secret token")

    async def start_sse(self) -> None:
        """Open the SSE stream after the handler has registered."""
        if not self.handler_id:
            raise RuntimeError("handler_id required before starting SSE")

        try:
            self._sse_reader, self._sse_writer = await asyncio.open_connection(
                self.host, self.port
            )
        except OSError as exc:
            raise ConnectionError(
                "Cannot connect to pacto-bot-api daemon via HTTP "
                f"at {self.name}: {exc}.\n"
                "Is the daemon running and reachable? "
                "Verify $PACTO_HTTP_BIND and that the daemon is listening."
            ) from exc

        request = (
            f"GET /events?handler_id={self.handler_id} HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            f"X-Pacto-Bot-Secret: {self.secret}\r\n"
            "Accept: text/event-stream\r\n"
            "\r\n"
        )
        self._sse_writer.write(request.encode("utf-8"))
        await self._sse_writer.drain()

        # Consume response headers.
        status = ""
        while True:
            line = await self._sse_reader.readline()
            if not line:
                raise ConnectionError(
                    "Cannot connect to pacto-bot-api daemon via HTTP "
                    f"at {self.name}: SSE connection closed while reading headers.\n"
                    "Is the daemon running and reachable? "
                    "Verify $PACTO_HTTP_BIND and that the daemon is listening."
                )
            line_str = line.decode("utf-8").rstrip("\r\n")
            if line_str == "":
                break
            if not status:
                status = line_str

        if not status or not status.startswith("HTTP/1.1 200"):
            raise ConnectionError(
                "Cannot connect to pacto-bot-api daemon via HTTP "
                f"at {self.name}: SSE request failed: {status}.\n"
                "Is the daemon running and reachable? "
                "Verify $PACTO_HTTP_BIND and that the daemon is listening."
            )

    async def readline(self) -> str:
        while self._sse_reader is None and not self._closed:
            await asyncio.sleep(0.05)
        if self._closed or self._sse_reader is None:
            return ""

        data_lines: list[str] = []
        while True:
            line = await self._sse_reader.readline()
            if not line:
                return ""
            line_str = line.decode("utf-8").rstrip("\r\n")
            if line_str == "":
                if data_lines:
                    break
                continue
            if line_str.startswith("data:"):
                data_lines.append(line_str[5:].lstrip())
            # ``event:`` lines and comments are ignored.

        return "".join(data_lines)

    async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
        method = frame.get("method", "")
        body = json.dumps(frame, separators=(",", ":")) + "\n"
        header_lines = [
            "POST / HTTP/1.1",
            f"Host: {self.host}:{self.port}",
            f"X-Pacto-Bot-Secret: {self.secret}",
            "Content-Type: application/json",
            f"Content-Length: {len(body.encode('utf-8'))}",
            "Connection: close",
        ]
        if method in self.MUTATING_METHODS and self.handler_id:
            header_lines.append(f"X-Pacto-Handler-Id: {self.handler_id}")
        request = "\r\n".join(header_lines) + "\r\n\r\n" + body

        reader, writer = await asyncio.open_connection(self.host, self.port)
        try:
            writer.write(request.encode("utf-8"))
            await writer.drain()
            return await self._read_response(reader)
        finally:
            writer.close()
            await writer.wait_closed()

    async def _read_response(
        self, reader: asyncio.StreamReader
    ) -> dict[str, Any] | None:
        status = ""
        content_length: int | None = None
        while True:
            line = await reader.readline()
            if not line:
                break
            line_str = line.decode("utf-8").rstrip("\r\n")
            if line_str == "":
                break
            if not status:
                status = line_str
            elif line_str.lower().startswith("content-length:"):
                content_length = int(line_str.split(":", 1)[1].strip())

        body = b""
        if content_length is not None:
            if content_length > 0:
                body = await reader.readexactly(content_length)
        else:
            # No Content-Length: read until the server closes the connection.
            body = await reader.read()

        for resp_line in body.decode("utf-8").splitlines():
            resp_line = resp_line.strip()
            if not resp_line:
                continue
            try:
                resp = json.loads(resp_line)
            except json.JSONDecodeError:
                continue
            if "id" in resp:
                return resp
        return None

    async def close(self) -> None:
        self._closed = True
        if self._sse_writer is not None:
            self._sse_writer.close()
            await self._sse_writer.wait_closed()
        self._sse_reader = None
        self._sse_writer = None


__all__ = [
    "Transport",
    "UnixTransport",
    "HttpTransport",
    "resolve_http_secret",
    "_resolve_socket_path",
    "_resolve_data_dir",
    "_resolve_http_bind",
]

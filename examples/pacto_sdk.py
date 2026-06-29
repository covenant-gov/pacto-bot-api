#!/usr/bin/env python3
"""Single-file SDK seed for pacto-bot-api example handlers.

This module is a hand-written helper so bot authors can focus on behavior
instead of copying the JSON-RPC framing and transport plumbing from
``echo_bot.py``. It uses only the Python standard library.

It is intentionally a *seed*, not the eventual generated Python client. The
generated client will be derived from ``schemas/jsonrpc.json`` and will provide
typed models, batching, and richer transports. See
``docs/plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md``
for the plan that defers the generated client.

Supported transports:

* Unix socket (default): derives the socket path from ``--socket`` /
  ``$PACTO_SOCKET``, ``--data-dir`` / ``$PACTO_DATA_DIR``, or the default
  ``~/.local/share/pacto-bot-api/pacto-bot-api.sock``.
* HTTP+SSE: connects to a loopback host:port (default ``127.0.0.1:9800``),
  registers via ``POST /``, consumes notifications via ``GET /events``, and
  attaches ``X-Pacto-Bot-Secret`` plus ``X-Pacto-Handler-Id`` on mutating
  methods.

Command syntax parsed from ``agent.event`` content:

    /command arg1 arg2 --flag value --bool

The leading ``/`` is stripped before registry lookup, so ``client.on('/hello',
handler)`` and ``client.on('hello', handler)`` match ``/hello`` and ``hello``
messages alike. Tokens starting with ``--`` are flags; if the next token does
not start with ``--`` it becomes the flag value, otherwise the flag is treated
as boolean ``True``.
"""

from __future__ import annotations

import argparse
import asyncio
import inspect
import json
import os
import signal
import sys
import uuid
from pathlib import Path
from typing import Any, Callable


# ---------------------------------------------------------------------------
# Defensive limits for command parsing
# ---------------------------------------------------------------------------

MAX_TOKENS = 256
MAX_TOKEN_BYTES = 1024
MAX_ARGS_FLAGS = 50


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _default_data_dir() -> str:
    return str(Path.home() / ".local" / "share" / "pacto-bot-api")


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


def _resolve_data_dir(data_dir: str | None) -> str:
    return data_dir or os.environ.get("PACTO_DATA_DIR") or _default_data_dir()


def _resolve_http_bind(http_bind: str | None) -> tuple[str, int]:
    value = http_bind or os.environ.get("PACTO_HTTP_BIND") or "127.0.0.1:9800"
    host, _, port_str = value.rpartition(":")
    if not host:
        host = "127.0.0.1"
    return host, int(port_str)


def _resolve_http_secret(secret: str | None, data_dir: str) -> str:
    if secret:
        return secret
    secret = os.environ.get("PACTO_SECRET_TOKEN", "")
    if secret:
        return secret
    token_path = Path(data_dir) / "bot_secret_token"
    if token_path.exists():
        return token_path.read_text().strip()
    raise RuntimeError(
        "HTTP transport requires a secret token via --secret, "
        "$PACTO_SECRET_TOKEN, or <data_dir>/bot_secret_token"
    )


def parse_command(content: str) -> dict[str, Any] | None:
    """Parse a small command grammar out of message content.

    Returns ``None`` for empty/whitespace content. The returned dict has keys
    ``command`` (str), ``args`` (list of str), and ``flags`` (dict of str to
    str or bool).
    """
    if not content or not content.strip():
        return None

    tokens = content.strip().split()
    if len(tokens) > MAX_TOKENS:
        tokens = tokens[:MAX_TOKENS]

    command = tokens[0].lstrip("/")
    args: list[str] = []
    flags: dict[str, str | bool] = {}

    i = 1
    while i < len(tokens):
        token = tokens[i]
        if len(token.encode("utf-8")) > MAX_TOKEN_BYTES:
            i += 1
            continue

        if token.startswith("--"):
            key = token[2:]
            if i + 1 < len(tokens) and not tokens[i + 1].startswith("--"):
                flags[key] = tokens[i + 1]
                i += 2
            else:
                flags[key] = True
                i += 1
        else:
            if len(args) < MAX_ARGS_FLAGS:
                args.append(token)
            i += 1

        if len(args) >= MAX_ARGS_FLAGS and len(flags) >= MAX_ARGS_FLAGS:
            break

    return {"command": command, "args": args, "flags": flags}


# ---------------------------------------------------------------------------
# Transports
# ---------------------------------------------------------------------------

class _Transport:
    """Common transport interface used by ``PactoClient``."""

    async def connect(self) -> None:
        raise NotImplementedError

    async def close(self) -> None:
        raise NotImplementedError

    async def readline(self) -> str:
        raise NotImplementedError

    async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
        raise NotImplementedError

    def set_handler_id(self, handler_id: str) -> None:
        pass

    async def start_sse(self) -> None:
        pass

    def __str__(self) -> str:
        return self.__class__.__name__


class UnixTransport(_Transport):
    """NDJSON-over-Unix-socket transport."""

    def __init__(self, socket_path: str):
        self.socket_path = socket_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None

    async def connect(self) -> None:
        self._reader, self._writer = await asyncio.open_unix_connection(
            self.socket_path
        )

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            await self._writer.wait_closed()
        self._reader = None
        self._writer = None

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

    def __str__(self) -> str:
        return f"unix:{self.socket_path}"


class HttpTransport(_Transport):
    """HTTP+SSE localhost transport using plain asyncio TCP streams.

    Outbound frames are sent as ``POST /`` with ``X-Pacto-Bot-Secret``.
    Mutating methods (``agent.send_dm``, ``agent.set_profile``,
    ``agent.error``) also include ``X-Pacto-Handler-Id``. Inbound daemon
    notifications are consumed from ``GET /events?handler_id=<id>`` as a
    text/event-stream.
    """

    MUTATING_METHODS = {"agent.send_dm", "agent.set_profile", "agent.error"}

    def __init__(self, host: str, port: int, secret: str):
        self.host = host
        self.port = port
        self.secret = secret
        self.handler_id: str | None = None
        self._sse_reader: asyncio.StreamReader | None = None
        self._sse_writer: asyncio.StreamWriter | None = None
        self._closed = False

    async def connect(self) -> None:
        if not self.secret:
            raise RuntimeError("HTTP transport requires a secret token")

    def set_handler_id(self, handler_id: str) -> None:
        self.handler_id = handler_id

    async def start_sse(self) -> None:
        if not self.handler_id:
            raise RuntimeError("handler_id required before starting SSE")

        self._sse_reader, self._sse_writer = await asyncio.open_connection(
            self.host, self.port
        )
        request = (
            f"GET /events?handler_id={self.handler_id} HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            f"X-Pacto-Bot-Secret: {self.secret}\r\n"
            "Accept: text/event-stream\r\n"
            "Connection: close\r\n"
            "\r\n"
        )
        self._sse_writer.write(request.encode("utf-8"))
        await self._sse_writer.drain()

        # Consume response headers.
        status = ""
        while True:
            line = await self._sse_reader.readline()
            if not line:
                raise ConnectionError("SSE connection closed while reading headers")
            line_str = line.decode("utf-8").rstrip("\r\n")
            if line_str == "":
                break
            if not status:
                status = line_str

        if not status or not status.startswith("HTTP/1.1 200"):
            raise ConnectionError(f"SSE request failed: {status}")

    async def readline(self) -> str:
        # The read loop may start before SSE is established (registration must
        # happen first). Poll until the SSE stream is ready or the transport
        # is closed.
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
            f"POST / HTTP/1.1",
            f"Host: {self.host}:{self.port}",
            f"X-Pacto-Bot-Secret: {self.secret}",
            "Content-Type: application/json",
            f"Content-Length: {len(body)}",
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

    async def _read_response(self, reader: asyncio.StreamReader) -> dict[str, Any] | None:
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

    def __str__(self) -> str:
        return f"http://{self.host}:{self.port}"


# ---------------------------------------------------------------------------
# Client
# ---------------------------------------------------------------------------

Handler = Callable[[dict[str, Any], "PactoClient"], Any]


class PactoClient:
    """Callback-based Pacto handler client.

    Example::

        client = PactoClient(bot_id="greeting-bot")

        @client.on("/hello")
        async def hello(event, client):
            return client.reply(event["event_id"], "Hello there!")

        client.on_default(lambda event, client: client.ignore(event["event_id"]))
        await client.run()

    Handlers may be sync or async. A handler returning ``None`` sends no
    response; otherwise the returned dict is sent as a ``handler.response``
    notification and must contain ``event_id`` and ``action``.
    """

    def __init__(
        self,
        bot_id: str = "echo-bot",
        event_types: list[str] | None = None,
        capabilities: list[str] | None = None,
        socket_path: str | None = None,
        data_dir: str | None = None,
        transport: str = "unix",
        secret: str | None = None,
        http_bind: str | None = None,
    ) -> None:
        self.bot_id = bot_id
        self.event_types = list(event_types or ["dm_received"])
        self.capabilities = list(capabilities or ["ReadMessages", "SendMessages"])
        self._data_dir = _resolve_data_dir(data_dir)
        self._transport = self._make_transport(
            socket_path, transport, secret, http_bind
        )
        self._registry: dict[str, Handler] = {}
        self._default_handler: Handler | None = None
        self._status_handler: Handler | None = None
        self._shutdown = asyncio.Event()
        self._pending: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._handler_id: str | None = None
        self._reader_task: asyncio.Task[None] | None = None

    def _make_transport(
        self,
        socket_path: str | None,
        transport: str,
        secret: str | None,
        http_bind: str | None,
    ) -> _Transport:
        transport = transport.lower()
        if transport == "unix":
            return UnixTransport(_resolve_socket_path(socket_path, self._data_dir))
        if transport == "http":
            host, port = _resolve_http_bind(http_bind)
            return HttpTransport(host, port, _resolve_http_secret(secret, self._data_dir))
        raise ValueError(f"unsupported transport: {transport}")

    def on(self, command: str, handler: Handler) -> Handler:
        """Register a handler for *command* (with or without leading ``/``)."""
        key = command.lstrip("/")
        self._registry[key] = handler
        return handler

    def on_default(self, handler: Handler) -> Handler:
        """Register a fallback handler for unrecognized commands."""
        self._default_handler = handler
        return handler

    def on_status(self, handler: Handler) -> Handler:
        """Register a callback for ``agent.status`` notifications."""
        self._status_handler = handler
        return handler

    def _log(self, message: str) -> None:
        print(f"[{self.bot_id}] {message}", file=sys.stderr, flush=True)

    async def run(self) -> None:
        """Connect, register, and run the inbound dispatch loop."""
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, self._request_shutdown)
            except (NotImplementedError, ValueError):
                pass

        await self._transport.connect()
        self._log(f"connected via {self._transport}")

        self._reader_task = asyncio.create_task(self._read_loop())

        try:
            response = await self._request(
                "handler.register",
                {
                    "bot_ids": [self.bot_id],
                    "event_types": self.event_types,
                    "capabilities": self.capabilities,
                },
            )
        except TimeoutError as exc:
            self._log(f"registration timed out: {exc}")
            await self._shutdown_gracefully()
            return

        if "error" in response:
            self._log(f"registration failed: {response['error']}")
            await self._shutdown_gracefully()
            return

        result = response.get("result", {})
        self._handler_id = result.get("handler_id")
        registered_events = result.get("registered_events", [])
        self._log(
            f"registered handler_id={self._handler_id} events={registered_events}"
        )

        self._transport.set_handler_id(self._handler_id or "")
        await self._transport.start_sse()

        await self._shutdown.wait()
        await self._shutdown_gracefully()

    def _request_shutdown(self) -> None:
        self._log("shutdown signal received")
        self._shutdown.set()

    async def _shutdown_gracefully(self) -> None:
        if self._reader_task is not None and not self._reader_task.done():
            self._reader_task.cancel()
            try:
                await self._reader_task
            except asyncio.CancelledError:
                pass
        await self._transport.close()
        self._log("disconnected")

    async def _request(
        self, method: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        request_id = str(uuid.uuid4())
        frame = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
        future: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        immediate = await self._transport.write_frame(frame)
        if immediate is not None:
            self._resolve_pending(request_id, immediate)
        try:
            return await asyncio.wait_for(future, timeout=10.0)
        except asyncio.TimeoutError as exc:
            self._pending.pop(request_id, None)
            raise TimeoutError(f"no response for {method}") from exc

    def _resolve_pending(self, request_id: str, response: dict[str, Any]) -> None:
        future = self._pending.pop(request_id, None)
        if future is not None and not future.done():
            future.set_result(response)

    async def _read_loop(self) -> None:
        try:
            while not self._shutdown.is_set():
                try:
                    line = await self._transport.readline()
                except asyncio.TimeoutError:
                    continue
                if not line:
                    self._log("transport closed")
                    break
                try:
                    frame = json.loads(line)
                except json.JSONDecodeError as exc:
                    self._log(f"invalid JSON: {exc}")
                    continue
                await self._handle_frame(frame)
        except asyncio.CancelledError:
            pass
        except Exception as exc:  # pragma: no cover - defensive
            self._log(f"read loop error: {exc}")

    async def _handle_frame(self, frame: dict[str, Any]) -> None:
        if "id" in frame:
            self._resolve_pending(str(frame["id"]), frame)
            return

        method = frame.get("method")
        params = frame.get("params", {})
        if method == "agent.event":
            await self._handle_event(params)
        elif method == "agent.status":
            await self._handle_status(params)
        else:
            self._log(f"unexpected notification: {method}")

    async def _handle_event(self, params: dict[str, Any]) -> None:
        event_id = params.get("event_id")
        content = params.get("content", "")
        parsed = parse_command(content)

        if parsed is None:
            self._log(f"ignoring malformed event {event_id}")
            await self.send("handler.response", self.ignore(event_id))
            return

        command = parsed["command"]
        handler = self._registry.get(command) or self._default_handler

        if handler is None:
            await self.send("handler.response", self.ignore(event_id))
            return

        try:
            if inspect.iscoroutinefunction(handler):
                result = await handler(params, self)
            else:
                result = handler(params, self)
        except Exception as exc:  # pragma: no cover - defensive
            self._log(f"handler error for {command}: {exc}")
            await self.send("handler.response", self.ignore(event_id))
            return

        if result is None:
            return

        if not isinstance(result, dict) or "event_id" not in result or "action" not in result:
            self._log(f"handler returned invalid response: {result!r}")
            await self.send("handler.response", self.ignore(event_id))
            return

        await self.send("handler.response", result)

    async def _handle_status(self, params: dict[str, Any]) -> None:
        if self._status_handler is not None:
            if inspect.iscoroutinefunction(self._status_handler):
                await self._status_handler(params, self)
            else:
                self._status_handler(params, self)
        else:
            self._log(f"daemon status: {params.get('state')}")

    async def send(self, method: str, params: dict[str, Any]) -> None:
        """Send a JSON-RPC notification."""
        frame = {"jsonrpc": "2.0", "method": method, "params": params}
        await self._transport.write_frame(frame)

    # Response helpers -------------------------------------------------------

    def ack(self, event_id: str) -> dict[str, Any]:
        return {"event_id": event_id, "action": "ack"}

    def reply(self, event_id: str, content: str) -> dict[str, Any]:
        return {"event_id": event_id, "action": "reply", "content": content}

    def defer(self, event_id: str) -> dict[str, Any]:
        return {"event_id": event_id, "action": "defer"}

    def ignore(self, event_id: str | None) -> dict[str, Any]:
        return {"event_id": event_id, "action": "ignore"}

    # Notification helpers ---------------------------------------------------

    def send_dm(
        self,
        bot_id: str,
        recipient: str,
        content: str,
        reply_to: str | None = None,
    ) -> dict[str, Any]:
        params: dict[str, Any] = {
            "bot_id": bot_id,
            "recipient": recipient,
            "content": content,
        }
        if reply_to is not None:
            params["reply_to"] = reply_to
        return params

    def set_profile(self, bot_id: str, **fields: str) -> dict[str, Any]:
        params: dict[str, Any] = {"bot_id": bot_id}
        for key in ("name", "about", "picture"):
            if key in fields:
                params[key] = fields[key]
        return params

    def error(
        self,
        bot_id: str,
        message: str,
        code: str | None = None,
        data: Any = None,
    ) -> dict[str, Any]:
        params: dict[str, Any] = {"bot_id": bot_id, "message": message}
        if code is not None:
            params["code"] = code
        if data is not None:
            params["data"] = data
        return params


# ---------------------------------------------------------------------------
# Convenience CLI helper
# ---------------------------------------------------------------------------

def add_sdk_arguments(parser: argparse.ArgumentParser) -> None:
    """Add common Pacto bot CLI flags to an ``ArgumentParser``."""
    parser.add_argument(
        "--socket",
        default=None,
        help="Path to the daemon Unix socket (default: $PACTO_SOCKET or "
        "$PACTO_DATA_DIR/pacto-bot-api.sock).",
    )
    parser.add_argument(
        "--data-dir",
        default=None,
        help="Data directory used to derive the default socket path.",
    )
    parser.add_argument(
        "--bot-id",
        default=os.environ.get("PACTO_BOT_ID", "echo-bot"),
        help="Bot identity to register for (default: echo-bot).",
    )
    parser.add_argument(
        "--transport",
        default=os.environ.get("PACTO_TRANSPORT", "unix"),
        choices=("unix", "http"),
        help="Transport to use (default: unix).",
    )
    parser.add_argument(
        "--http-bind",
        default=os.environ.get("PACTO_HTTP_BIND"),
        help="HTTP bind address (default: $PACTO_HTTP_BIND or 127.0.0.1:9800).",
    )
    parser.add_argument(
        "--secret",
        default=os.environ.get("PACTO_SECRET_TOKEN"),
        help="HTTP secret token (default: $PACTO_SECRET_TOKEN or "
        "<data_dir>/bot_secret_token).",
    )

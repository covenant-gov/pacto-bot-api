"""Focused HTTP+SSE test for the greeting bot using the SDK.

Starts ``greeting_bot.py`` with ``--transport http`` against a minimal mock
HTTP daemon and verifies that:

1. The bot registers via ``POST /`` with the correct secret.
2. The bot consumes ``GET /events`` as an SSE stream.
3. An injected ``/hello`` event produces a ``handler.response`` reply.
4. The reply is received over a subsequent ``POST /`` request.
"""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest


class _MockHttpDaemon:
    """Minimal HTTP daemon that implements the subset of routes the SDK uses."""

    def __init__(self) -> None:
        self.secret = "test-secret-token"
        self.host = "127.0.0.1"
        self.port = 0
        self.server: asyncio.Server | None = None
        self.handler_id: str | None = None
        self.responses: list[dict[str, Any]] = []
        self.event_sent = asyncio.Event()
        self.response_received = asyncio.Event()

    async def start(self) -> int:
        self.server = await asyncio.start_server(
            self._handle, host=self.host, port=self.port
        )
        self.port = self.server.sockets[0].getsockname()[1]
        return self.port

    async def stop(self) -> None:
        if self.server is not None:
            self.server.close()
            await self.server.wait_closed()

    async def _read_request(self, reader: asyncio.StreamReader) -> tuple[str, dict[str, str], bytes]:
        header_data = b""
        while True:
            line = await reader.readline()
            if not line:
                break
            header_data += line
            if line == b"\r\n":
                break

        headers: dict[str, str] = {}
        lines = header_data.decode("utf-8").split("\r\n")
        request_line = lines[0]
        for line in lines[1:]:
            if ":" in line:
                key, value = line.split(":", 1)
                headers[key.strip().lower()] = value.strip()

        body = b""
        content_length = int(headers.get("content-length", "0"))
        if content_length > 0:
            body = await reader.readexactly(content_length)

        return request_line, headers, body

    async def _send(
        self,
        writer: asyncio.StreamWriter,
        status: str,
        body: bytes = b"",
        content_type: str = "text/plain; charset=utf-8",
    ) -> None:
        response = (
            f"HTTP/1.1 {status}\r\n"
            f"Content-Type: {content_type}\r\n"
            f"Content-Length: {len(body)}\r\n"
            "Connection: close\r\n"
            "\r\n"
        ).encode("utf-8") + body
        writer.write(response)
        await writer.drain()

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            request_line, headers, body = await self._read_request(reader)
            provided_secret = headers.get("x-pacto-bot-secret", "")
            if provided_secret != self.secret:
                await self._send(writer, "401 Unauthorized")
                return

            if request_line.startswith("POST /"):
                await self._handle_post(writer, headers, body)
            elif request_line.startswith("GET /events"):
                await self._handle_events(writer)
            else:
                await self._send(writer, "404 Not Found")
        finally:
            writer.close()
            await writer.wait_closed()

    async def _handle_post(
        self,
        writer: asyncio.StreamWriter,
        headers: dict[str, str],
        body: bytes,
    ) -> None:
        try:
            frame = json.loads(body.decode("utf-8"))
        except json.JSONDecodeError:
            await self._send(writer, "400 Bad Request")
            return

        method = frame.get("method")
        if method == "handler.register":
            self.handler_id = "test-handler-123"
            response = {
                "jsonrpc": "2.0",
                "id": frame.get("id"),
                "result": {
                    "handler_id": self.handler_id,
                    "registered_events": ["dm_received"],
                },
            }
            body_out = (json.dumps(response, separators=(",", ":")) + "\n").encode()
            await self._send(writer, "200 OK", body_out)
            return

        # Mutating methods require the handler id header.
        if method in {"agent.send_dm", "agent.set_profile", "agent.error"}:
            if headers.get("x-pacto-handler-id") != self.handler_id:
                await self._send(writer, "401 Unauthorized")
                return

        if method == "handler.response":
            self.responses.append(frame)
            self.response_received.set()

        await self._send(writer, "200 OK", b"")

    async def _handle_events(self, writer: asyncio.StreamWriter) -> None:
        headers = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/event-stream\r\n"
            "Connection: close\r\n"
            "\r\n"
        )
        writer.write(headers.encode("utf-8"))
        await writer.drain()

        event = {
            "jsonrpc": "2.0",
            "method": "agent.event",
            "params": {
                "bot_id": "echo-bot",
                "event_id": "http-greet-0001",
                "type": "dm_received",
                "chat_id": None,
                "content": "/hello",
                "rumor_id": "rumor-http-greet-0001",
                "author": "npub1sender",
                "timestamp": 1700000000000,
            },
        }
        data = json.dumps(event, separators=(",", ":"))
        writer.write(f"event: agent.event\ndata: {data}\n\n".encode("utf-8"))
        await writer.drain()
        self.event_sent.set()

        # Keep the SSE stream open until the bot has had time to respond.
        try:
            await asyncio.wait_for(self.response_received.wait(), timeout=3.0)
        except asyncio.TimeoutError:
            pass


@pytest.mark.asyncio
async def test_greeting_bot_over_http() -> None:
    daemon = _MockHttpDaemon()
    port = await daemon.start()
    bot_file = Path(__file__).with_name("greeting_bot.py")
    proc: subprocess.Popen[str] | None = None

    try:
        proc = subprocess.Popen(
            [
                sys.executable,
                str(bot_file),
                "--transport",
                "http",
                "--http-bind",
                f"127.0.0.1:{port}",
                "--secret",
                daemon.secret,
                "--bot-id",
                "echo-bot",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        try:
            await asyncio.wait_for(daemon.response_received.wait(), timeout=10.0)
        except asyncio.TimeoutError as exc:
            stderr = proc.stderr.read() if proc.stderr else ""
            raise AssertionError(
                f"greeting_bot did not respond over HTTP within 10s\nSTDERR:\n{stderr}"
            ) from exc

        assert len(daemon.responses) == 1, daemon.responses
        params = daemon.responses[0].get("params", {})
        assert params.get("event_id") == "http-greet-0001"
        assert params.get("action") == "reply"
        assert "Hello" in params.get("content", "")
    finally:
        await daemon.stop()
        if proc is not None and proc.poll() is None:
            proc.send_signal(2)  # SIGINT
            try:
                proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5.0)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])

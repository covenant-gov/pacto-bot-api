#!/usr/bin/env python3
"""Reference Pacto handler: echoes DMs that start with '/echo'.

Connects to the pacto-bot-api daemon over its Unix socket, registers for
``dm_received`` events on ``echo-bot``, and replies to any message whose
content starts with ``/echo ``.  Other messages are ignored.

Usage:
    python echo_bot.py
    python echo_bot.py --socket /run/pacto-bot-api.sock
    python echo_bot.py --data-dir ~/.local/share/pacto-bot-api

This file uses only the Python standard library.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import signal
import sys
import uuid
from pathlib import Path
from typing import Any


def _default_data_dir() -> str:
    home = Path.home()
    return str(home / ".local" / "share" / "pacto-bot-api")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Reference echo handler for pacto-bot-api."
    )
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
        "--event-types",
        default="dm_received",
        help="Comma-separated event types to subscribe to (default: dm_received).",
    )
    parser.add_argument(
        "--capabilities",
        default="ReadMessages,SendMessages",
        help="Comma-separated capabilities to request (default: ReadMessages,SendMessages).",
    )
    return parser.parse_args(argv)


class EchoHandler:
    """Async JSON-RPC handler for the Pacto daemon."""

    def __init__(
        self,
        socket_path: str,
        bot_id: str,
        event_types: list[str],
        capabilities: list[str],
    ) -> None:
        self.socket_path = socket_path
        self.bot_id = bot_id
        self.event_types = event_types
        self.capabilities = capabilities
        self._shutdown = asyncio.Event()
        self._writer_task: asyncio.Task[None] | None = None
        self._outbound: asyncio.Queue[dict[str, Any]] = asyncio.Queue()

    async def run(self) -> None:
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, self._request_shutdown)
            except (NotImplementedError, ValueError):
                # Windows or already handled; ignore.
                pass

        try:
            reader, writer = await asyncio.open_unix_connection(self.socket_path)
        except OSError as exc:
            self._log(f"failed to connect to {self.socket_path}: {exc}")
            raise

        self._log(f"connected to {self.socket_path}")

        await self._register()
        self._writer_task = asyncio.create_task(self._write_loop(writer))
        await self._read_loop(reader)

        self._shutdown.set()
        if self._writer_task is not None:
            self._writer_task.cancel()
            try:
                await self._writer_task
            except asyncio.CancelledError:
                pass
        writer.close()
        await writer.wait_closed()
        self._log("disconnected")

    def _request_shutdown(self) -> None:
        self._log("shutdown signal received")
        self._shutdown.set()

    async def _write_loop(self, writer: asyncio.StreamWriter) -> None:
        while not self._shutdown.is_set():
            try:
                msg = await asyncio.wait_for(self._outbound.get(), timeout=0.2)
            except asyncio.TimeoutError:
                continue
            line = json.dumps(msg, separators=(",", ":")) + "\n"
            writer.write(line.encode())
            await writer.drain()

    async def _read_loop(self, reader: asyncio.StreamReader) -> None:
        while not self._shutdown.is_set():
            try:
                line = await asyncio.wait_for(reader.readline(), timeout=0.5)
            except asyncio.TimeoutError:
                continue
            if not line:
                self._log("daemon closed connection")
                break
            await self._handle_line(line.decode().strip())

    async def _handle_line(self, line: str) -> None:
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as exc:
            self._log(f"invalid JSON: {exc}")
            return

        method = msg.get("method")
        if method == "agent.status":
            params = msg.get("params") or {}
            self._log(f"daemon status: {params.get('state')}")
            return

        if method == "agent.event":
            await self._handle_event(msg.get("params") or {})
            return

        # Responses to our requests (e.g. handler.register).
        if "id" in msg:
            if "result" in msg:
                self._log(f"rpc result: {msg['result']}")
            elif "error" in msg:
                self._log(f"rpc error: {msg['error']}")

    async def _handle_event(self, params: dict[str, Any]) -> None:
        event_id = params.get("event_id")
        content = params.get("content", "")
        bot_id = params.get("bot_id", self.bot_id)
        self._log(f"event {event_id} for {bot_id}: {content!r}")

        if content.startswith("/echo "):
            reply_text = "Echo: " + content[len("/echo "):]
            self._send_notification(
                "handler.response",
                {"event_id": event_id, "action": "reply", "content": reply_text},
            )
            self._log(f"replying to {event_id}: {reply_text!r}")
        else:
            self._send_notification(
                "handler.response",
                {"event_id": event_id, "action": "ignore"},
            )
            self._log(f"ignoring {event_id}")

    async def _register(self) -> None:
        request_id = str(uuid.uuid4())
        msg = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "handler.register",
            "params": {
                "bot_ids": [self.bot_id],
                "event_types": self.event_types,
                "capabilities": self.capabilities,
            },
        }
        self._outbound.put_nowait(msg)
        self._log(
            f"registering for bot={self.bot_id} events={self.event_types} "
            f"caps={self.capabilities}"
        )

    def _send_notification(self, method: str, params: dict[str, Any]) -> None:
        self._outbound.put_nowait(
            {"jsonrpc": "2.0", "method": method, "params": params}
        )

    def _log(self, message: str) -> None:
        print(f"[echo-bot] {message}", file=sys.stderr, flush=True)


async def main(argv: list[str] | None = None) -> None:
    args = parse_args(argv)
    data_dir = args.data_dir or os.environ.get("PACTO_DATA_DIR", "")
    socket_path = args.socket or os.environ.get("PACTO_SOCKET", "")
    if not socket_path:
        socket_path = str(Path(data_dir or _default_data_dir()) / "pacto-bot-api.sock")

    handler = EchoHandler(
        socket_path=socket_path,
        bot_id=args.bot_id,
        event_types=[t.strip() for t in args.event_types.split(",") if t.strip()],
        capabilities=[c.strip() for c in args.capabilities.split(",") if c.strip()],
    )
    await handler.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)

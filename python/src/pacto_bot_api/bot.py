"""Decorator-based high-level bot layer for the pacto-bot-api Python SDK."""

from __future__ import annotations

import argparse
import asyncio
import inspect
import os
import signal
import sys
from typing import Any, Callable

from ._generated.client import PactoClient, PactoClientError
from ._generated.models import AgentEventParams, AgentStatusParams
from .parser import parse_command
from .transports import (
    HttpTransport,
    Transport,
    UnixTransport,
    _resolve_data_dir,
    _resolve_http_bind,
    _resolve_socket_path,
    resolve_http_secret,
)


CommandHandler = Callable[[AgentEventParams, "Bot"], Any]
StatusHandler = Callable[[AgentStatusParams, "Bot"], Any]


class Bot:
    """Decorator-based bot built on the generated PactoClient.

    Example::

        from pacto_bot_api import Bot

        bot = Bot(bot_id="greeting-bot")

        @bot.command("/hello")
        async def hello(event, bot):
            return {"event_id": event.event_id, "action": "reply", "content": "Hi!"}

        if __name__ == "__main__":
            bot.run()

    Transport settings are resolved with the same precedence as the hand-written
    seed SDK: explicit constructor argument → CLI flag → environment variable →
    default.
    """

    def __init__(
        self,
        bot_id: str,
        transport: Transport | str | None = None,
        event_types: list[str] | None = None,
        capabilities: list[str] | None = None,
        socket_path: str | None = None,
        data_dir: str | None = None,
        secret: str | None = None,
        http_bind: str | None = None,
        reply_on_error: bool = True,
        error_message: str = "Sorry, I couldn't process that.",
    ) -> None:
        self.bot_id = bot_id
        self.event_types = list(event_types or ["dm_received"])
        self.capabilities = list(capabilities or ["ReadMessages", "SendMessages"])
        self.reply_on_error = reply_on_error
        self.error_message = error_message

        self._data_dir = _resolve_data_dir(data_dir)
        # Stash constructor-provided settings so CLI args can override them in run().
        self._transport_arg = transport
        self._socket_path_arg = socket_path
        self._secret_arg = secret
        self._http_bind_arg = http_bind

        self._transport = self._make_transport(
            transport, socket_path, secret, http_bind, self._data_dir
        )
        self._client = PactoClient(self._transport)

        self._commands: dict[str, CommandHandler] = {}
        self._default_handler: CommandHandler | None = None
        self._status_handler: StatusHandler | None = None

        self._shutdown = asyncio.Event()
        self._reader_task: asyncio.Task[None] | None = None
        self._handler_id: str | None = None

        self._install_signal_handlers()

    def _make_transport(
        self,
        transport: Transport | str | None,
        socket_path: str | None,
        secret: str | None,
        http_bind: str | None,
        data_dir: str,
    ) -> Transport:
        if isinstance(transport, Transport):
            return transport

        transport_name = (transport or os.environ.get("PACTO_TRANSPORT", "unix")).lower()
        if transport_name == "unix":
            return UnixTransport(_resolve_socket_path(socket_path, data_dir))
        if transport_name == "http":
            host, port = _resolve_http_bind(http_bind)
            return HttpTransport(
                host,
                port,
                resolve_http_secret(secret, data_dir),
            )
        raise ValueError(f"unsupported transport: {transport_name}")

    def _install_signal_handlers(self) -> None:
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                loop.add_signal_handler(sig, self._request_shutdown)
            except (NotImplementedError, ValueError):
                pass

    def _request_shutdown(self) -> None:
        self._log("shutdown signal received")
        self._shutdown.set()

    def _log(self, message: str) -> None:
        print(f"[{self.bot_id}] {message}", file=sys.stderr, flush=True)

    # -----------------------------------------------------------------------
    # Decorators
    # -----------------------------------------------------------------------

    def command(self, name: str) -> Callable[[CommandHandler], CommandHandler]:
        """Register an async callback for *name* (with or without leading ``/``)."""
        key = name.lstrip("/")

        def decorator(handler: CommandHandler) -> CommandHandler:
            self._commands[key] = handler
            return handler

        return decorator

    def default(self, handler: CommandHandler) -> CommandHandler:
        """Register a fallback callback for unrecognized commands."""
        self._default_handler = handler
        return handler

    def status(self, handler: StatusHandler) -> StatusHandler:
        """Register a callback for ``agent.status`` notifications."""
        self._status_handler = handler
        return handler

    # -----------------------------------------------------------------------
    # Helpers exposed to handlers
    # -----------------------------------------------------------------------

    @property
    def client(self) -> PactoClient:
        """The underlying generated client."""
        return self._client

    async def send_dm(
        self,
        recipient: str,
        content: str,
        reply_to: str | None = None,
    ) -> str:
        """Send a direct message as this bot."""
        return await self._client.agent_send_dm(
            bot_id=self.bot_id,
            recipient=recipient,
            content=content,
            reply_to=reply_to,
        )

    async def set_profile(
        self,
        name: str | None = None,
        about: str | None = None,
        picture: str | None = None,
    ) -> str:
        """Update this bot's Nostr kind:0 profile."""
        return await self._client.agent_set_profile(
            bot_id=self.bot_id,
            name=name,
            about=about,
            picture=picture,
        )

    # -----------------------------------------------------------------------
    # Run loop
    # -----------------------------------------------------------------------

    def run(self, argv: list[str] | None = None) -> None:
        """Parse CLI args, connect, register, and run the dispatch loop."""
        try:
            asyncio.run(self._run(argv))
        except KeyboardInterrupt:
            sys.exit(0)

    async def _run(self, argv: list[str] | None = None) -> None:
        args = self._parse_args(argv)

        # If a transport instance was passed to the constructor, it wins.
        # Otherwise re-resolve with CLI args overriding constructor args,
        # which in turn override environment variables and defaults.
        if not isinstance(self._transport_arg, Transport):
            transport: Transport | str | None = self._transport_arg
            if args.transport is not None:
                transport = args.transport
            socket_path = args.socket if args.socket is not None else self._socket_path_arg
            secret = args.secret if args.secret is not None else self._secret_arg
            http_bind = args.http_bind if args.http_bind is not None else self._http_bind_arg
            data_dir = args.data_dir if args.data_dir is not None else self._data_dir
            self._transport = self._make_transport(
                transport, socket_path, secret, http_bind, data_dir
            )
            self._client = PactoClient(self._transport)
        # Re-install signal handlers now that we have an event loop.
        self._install_signal_handlers()

        await self._client.connect()
        self._log(f"connected via {self._transport.name}")

        try:
            result = await self._client.handler_register(
                bot_ids=[self.bot_id],
                event_types=self.event_types,
                capabilities=self.capabilities,
            )
        except (PactoClientError, TimeoutError) as exc:
            self._log(f"registration failed: {exc}")
            await self._client.close()
            return

        self._handler_id = result.handler_id
        self._log(
            f"registered handler_id={self._handler_id} events={result.registered_events}"
        )

        # Tell HTTP transports the handler id so mutating calls and SSE work.
        if isinstance(self._transport, HttpTransport):
            self._transport.handler_id = self._handler_id
            await self._transport.start_sse()

        self._reader_task = asyncio.create_task(self._dispatch_loop())

        await self._shutdown.wait()
        await self._shutdown_gracefully()

    def _parse_args(self, argv: list[str] | None) -> argparse.Namespace:
        parser = argparse.ArgumentParser(description=f"Pacto bot: {self.bot_id}")
        parser.add_argument(
            "--socket",
            default=None,
            help="Path to the daemon Unix socket.",
        )
        parser.add_argument(
            "--data-dir",
            default=None,
            help="Data directory used to derive defaults.",
        )
        parser.add_argument(
            "--transport",
            default=None,
            help="Transport to use (unix or http). Defaults to $PACTO_TRANSPORT or unix.",
        )
        parser.add_argument(
            "--http-bind",
            default=None,
            help="HTTP bind address (default: $PACTO_HTTP_BIND or 127.0.0.1:9800).",
        )
        parser.add_argument(
            "--secret",
            default=None,
            help="HTTP secret token (default: $PACTO_SECRET_TOKEN).",
        )
        return parser.parse_args(argv)

    async def _shutdown_gracefully(self) -> None:
        if self._reader_task is not None and not self._reader_task.done():
            self._reader_task.cancel()
            try:
                await self._reader_task
            except asyncio.CancelledError:
                pass
        await self._client.close()
        self._log("disconnected")

    async def _dispatch_loop(self) -> None:
        try:
            async for notification in self._client.notifications():
                if isinstance(notification, AgentEventParams):
                    await self._handle_event(notification)
                elif isinstance(notification, AgentStatusParams):
                    await self._handle_status(notification)
        except asyncio.CancelledError:
            pass

    async def _handle_event(self, event: AgentEventParams) -> None:
        parsed = parse_command(event.content)

        if parsed is None:
            self._log(f"ignoring malformed event {event.event_id}")
            await self._client.handler_response(
                action="ignore", event_id=event.event_id
            )
            return

        command = parsed["command"]
        handler = self._commands.get(command) or self._default_handler

        if handler is None:
            await self._client.handler_response(
                action="ignore", event_id=event.event_id
            )
            return

        try:
            result = handler(event, self)
            if inspect.isawaitable(result):
                result = await result
        except Exception as exc:  # pragma: no cover - defensive
            self._log(f"handler error for {command}: {exc}")
            if self.reply_on_error:
                await self._client.handler_response(
                    action="reply",
                    event_id=event.event_id,
                    content=self.error_message,
                )
            else:
                await self._client.handler_response(
                    action="ignore", event_id=event.event_id
                )
            return

        if result is None:
            return

        if not isinstance(result, dict) or "event_id" not in result or "action" not in result:
            self._log(f"handler returned invalid response: {result!r}")
            await self._client.handler_response(
                action="ignore", event_id=event.event_id
            )
            return

        await self._client.handler_response(
            action=result["action"],
            event_id=result["event_id"],
            content=result.get("content"),
        )

    async def _handle_status(self, status: AgentStatusParams) -> None:
        if self._status_handler is not None:
            result = self._status_handler(status, self)
            if inspect.isawaitable(result):
                await result
        else:
            self._log(f"daemon status: {status.state}")


__all__ = ["Bot"]

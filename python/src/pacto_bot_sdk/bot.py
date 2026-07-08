"""Decorator-based high-level bot layer for the pacto-bot-sdk Python SDK."""

from __future__ import annotations

import argparse
import asyncio
import inspect
import json
import os
import signal
import sys
import traceback
from typing import Any, Callable

from ._generated.client import PactoClient, PactoClientError
from ._generated.models import AgentEventParams, AgentRateLimitedParams, AgentStatusParams
from .logger import Logger
from .parser import parse_command
from .retry_circuit import RetryCircuit
from .transports import (
    AutoTransport,
    HttpTransport,
    Transport,
    TransportDisconnected,
    UnixTransport,
    _resolve_data_dir,
    _resolve_http_bind,
    _resolve_socket_path,
    resolve_http_secret,
)


CommandHandler = Callable[[AgentEventParams, "Bot"], Any]
StatusHandler = Callable[[AgentStatusParams, "Bot"], Any]
RateLimitedHandler = Callable[[AgentRateLimitedParams, "Bot"], Any]


class Bot:
    """Decorator-based bot built on the generated PactoClient.

    Example::

        from pacto_bot_sdk import Bot

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
        retry_initial_backoff: float = 1.0,
        retry_max_backoff: float = 30.0,
        retry_jitter_ratio: float = 0.2,
        circuit_failure_threshold: int = 5,
        circuit_cooling_off_seconds: float = 60.0,
        degraded_log_interval: float = 60.0,
        log_level: str | None = None,
    ) -> None:
        self.bot_id = bot_id
        self.event_types = list(event_types or ["dm_received"])
        self.capabilities = list(capabilities or ["ReadMessages", "SendMessages"])
        self.reply_on_error = reply_on_error
        self.error_message = error_message

        # Logger is created first so every later step can emit diagnostics.
        self._logger = Logger(bot_id, log_level)

        # Retry/circuit-breaker settings: constructor args are stashed so CLI
        # args can override them in run().
        self._retry_initial_backoff_arg = retry_initial_backoff
        self._retry_max_backoff_arg = retry_max_backoff
        self._retry_jitter_ratio_arg = retry_jitter_ratio
        self._circuit_failure_threshold_arg = circuit_failure_threshold
        self._circuit_cooling_off_seconds_arg = circuit_cooling_off_seconds
        self._degraded_log_interval_arg = degraded_log_interval

        self._data_dir = _resolve_data_dir(data_dir)
        # Stash constructor-provided settings so CLI args can override them in run().
        self._transport_arg = transport
        self._socket_path_arg = socket_path
        self._secret_arg = secret
        self._http_bind_arg = http_bind

        self._transport = self._make_transport(
            transport, socket_path, secret, http_bind, self._data_dir, self._logger
        )
        self._client = PactoClient(self._transport)

        self._commands: dict[str, CommandHandler] = {}
        self._default_handler: CommandHandler | None = None
        self._status_handler: StatusHandler | None = None
        self._rate_limited_handler: RateLimitedHandler | None = None

        self._shutdown = asyncio.Event()
        self._reader_task: asyncio.Task[None] | None = None
        self._handler_id: str | None = None
        self._reconnect_token: str | None = None

        self._install_signal_handlers()

        # Validate retry/circuit settings supplied at construction time so that
        # programmer errors surface immediately rather than at runtime.
        self._make_retry_circuit(
            retry_initial_backoff,
            retry_max_backoff,
            retry_jitter_ratio,
            circuit_failure_threshold,
            circuit_cooling_off_seconds,
            degraded_log_interval,
        )

    def _make_retry_circuit(
        self,
        retry_initial_backoff: float,
        retry_max_backoff: float,
        retry_jitter_ratio: float,
        circuit_failure_threshold: int,
        circuit_cooling_off_seconds: float,
        degraded_log_interval: float,
    ) -> RetryCircuit:
        return RetryCircuit(
            retry_initial_backoff=retry_initial_backoff,
            retry_max_backoff=retry_max_backoff,
            retry_jitter_ratio=retry_jitter_ratio,
            circuit_failure_threshold=circuit_failure_threshold,
            circuit_cooling_off_seconds=circuit_cooling_off_seconds,
            degraded_log_interval=degraded_log_interval,
        )

    def _resolve_retry_settings(self, args: argparse.Namespace) -> RetryCircuit:
        """Resolve retry/circuit settings with CLI precedence over constructor args."""
        return self._make_retry_circuit(
            retry_initial_backoff=self._first(
                args.retry_initial_backoff, self._retry_initial_backoff_arg
            ),
            retry_max_backoff=self._first(
                args.retry_max_backoff, self._retry_max_backoff_arg
            ),
            retry_jitter_ratio=self._first(
                args.retry_jitter_ratio, self._retry_jitter_ratio_arg
            ),
            circuit_failure_threshold=self._first(
                args.circuit_failure_threshold, self._circuit_failure_threshold_arg
            ),
            circuit_cooling_off_seconds=self._first(
                args.circuit_cooling_off_seconds, self._circuit_cooling_off_seconds_arg
            ),
            degraded_log_interval=self._first(
                args.degraded_log_interval, self._degraded_log_interval_arg
            ),
        )

    @staticmethod
    def _first(value, fallback):
        """Return ``value`` if it is not None, otherwise ``fallback``."""
        return value if value is not None else fallback

    def _make_transport(
        self,
        transport: Transport | str | None,
        socket_path: str | None,
        secret: str | None,
        http_bind: str | None,
        data_dir: str,
        logger: Logger | None = None,
    ) -> Transport:
        logger = logger or Logger(self.bot_id, None)
        if isinstance(transport, Transport):
            if hasattr(transport, "logger"):
                transport.logger = logger
            return transport

        transport_name = (transport or os.environ.get("PACTO_TRANSPORT", "unix")).lower()
        if transport_name == "auto":
            return AutoTransport(
                _resolve_socket_path(socket_path, data_dir),
                http_bind,
                data_dir,
                logger=logger,
            )
        if transport_name == "unix":
            return UnixTransport(
                _resolve_socket_path(socket_path, data_dir), logger=logger
            )
        if transport_name == "http":
            host, port = _resolve_http_bind(http_bind)
            return HttpTransport(
                host,
                port,
                resolve_http_secret(secret, data_dir),
                logger=logger,
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
        self._logger.info("shutdown signal received")
        self._shutdown.set()

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

    def rate_limited(self, handler: RateLimitedHandler) -> RateLimitedHandler:
        """Register a callback for ``agent.rate_limited`` notifications."""
        self._rate_limited_handler = handler
        return handler

    # -----------------------------------------------------------------------
    # Helpers exposed to handlers
    # -----------------------------------------------------------------------

    @property
    def client(self) -> PactoClient:
        """The underlying generated client."""
        return self._client

    def log(self, message: str, level: str = "info") -> None:
        """Emit a log message at the given level.

        ``level`` must be one of ``debug``, ``info``, ``warn``, or ``error``.
        """
        self._logger.log(level, message)

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
    # Retry/circuit helpers
    # -----------------------------------------------------------------------

    @property
    def is_degraded(self) -> bool:
        """True when the circuit breaker is open and the bot is not dispatching."""
        return self._retry_circuit.is_open if hasattr(self, "_retry_circuit") else False

    def _log_degraded_open(self) -> None:
        """Log once when the circuit breaker opens."""
        self._logger.warn(
            f"degraded: {self._transport.name} failed "
            f"{self._retry_circuit.failure_count} time(s); "
            f"cooling off for {self._retry_circuit.circuit_cooling_off_seconds}s"
        )

    def _log_degraded_status(self) -> None:
        """Log a periodic status line while the circuit remains open."""
        now = asyncio.get_running_loop().time()
        interval = self._retry_circuit.degraded_log_interval
        if interval == 0:
            return
        if self._degraded_logged_at is None or (now - self._degraded_logged_at) >= interval:
            self._logger.warn("degraded: still waiting for daemon")
            self._degraded_logged_at = now

    def _log_degraded_recovered(self) -> None:
        """Log when the circuit closes."""
        self._logger.info("degraded: recovered")
        self._degraded_logged_at = None

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
        if args.log_level is not None:
            self._logger.set_level(args.log_level)
        self._retry_circuit = self._resolve_retry_settings(args)
        self._degraded_logged_at: float | None = None

        # Re-install signal handlers now that we have an event loop.
        self._install_signal_handlers()

        while not self._shutdown.is_set():
            should_attempt, wait = self._retry_circuit.next_action()
            if not should_attempt:
                self._log_degraded_status()
                try:
                    await asyncio.wait_for(self._shutdown.wait(), timeout=wait)
                except asyncio.TimeoutError:
                    # Cooling-off elapsed; next loop will be half-open.
                    continue
                else:
                    break

            if wait > 0:
                self._logger.warn(f"reconnecting in {wait}s...")
                try:
                    await asyncio.wait_for(self._shutdown.wait(), timeout=wait)
                except asyncio.TimeoutError:
                    pass
                else:
                    break

            try:
                await self._run_once(args)
            except (OSError, TimeoutError, asyncio.TimeoutError, PactoClientError, TransportDisconnected) as exc:
                if self._shutdown.is_set():
                    break
                self._logger.error(f"connection lost: {exc}")
                was_open = self._retry_circuit.is_open
                self._retry_circuit.record_failure()
                if not was_open and self._retry_circuit.is_open:
                    self._log_degraded_open()
            else:
                # _run_once only returns cleanly when shutdown is requested.
                was_closed = self._retry_circuit.is_closed
                self._retry_circuit.record_success()
                if not was_closed:
                    self._log_degraded_recovered()
                if self._shutdown.is_set():
                    break
                # Defensive: an unexpected clean return means the dispatch loop
                # ended without an explicit shutdown signal, so treat it as a
                # disconnect.
                self._logger.error("connection lost: daemon disconnected")
                was_open = self._retry_circuit.is_open
                self._retry_circuit.record_failure()
                if not was_open and self._retry_circuit.is_open:
                    self._log_degraded_open()

        await self._close_client()

    async def _close_client(self) -> None:
        if self._client is not None:
            await self._client.close()

    def _resolve_client(self, args: argparse.Namespace) -> PactoClient:
        transport: Transport | str | None = self._transport_arg
        if args.transport is not None:
            transport = args.transport
        socket_path = args.socket if args.socket is not None else self._socket_path_arg
        secret = args.secret if args.secret is not None else self._secret_arg
        http_bind = args.http_bind if args.http_bind is not None else self._http_bind_arg
        data_dir = args.data_dir if args.data_dir is not None else self._data_dir
        self._transport = self._make_transport(
            transport, socket_path, secret, http_bind, data_dir, self._logger
        )
        return PactoClient(self._transport)

    async def _run_once(self, args: argparse.Namespace) -> None:
        # Close any previous client before creating a fresh one. For built-in
        # transports the transport is recreated below; for custom transport
        # instances we reuse the same transport but create a fresh PactoClient
        # so the generated read loop is not permanently disabled by a prior
        # close().
        await self._close_client()
        if not isinstance(self._transport_arg, Transport):
            self._client = self._resolve_client(args)
        else:
            self._client = PactoClient(self._transport)

        try:
            await self._client.connect()
        except (OSError, TimeoutError, asyncio.TimeoutError) as exc:
            self._logger.error(str(exc))
            raise

        self._logger.info(f"connected via {self._transport.name}")

        try:
            if self._handler_id and self._reconnect_token:
                result = await self._client.handler_reconnect(
                    handler_id=self._handler_id,
                    reconnect_token=self._reconnect_token,
                )
            else:
                result = await self._client.handler_register(
                    bot_ids=[self.bot_id],
                    event_types=self.event_types,
                    capabilities=self.capabilities,
                )
                self._reconnect_token = result.reconnect_token
        except (PactoClientError, TimeoutError, asyncio.TimeoutError) as exc:
            self._logger.error(f"registration failed: {exc}")
            raise

        self._handler_id = result.handler_id
        self._logger.info(
            f"registered handler_id={self._handler_id} events={result.registered_events}"
        )

        # Tell HTTP transports the handler id so mutating calls and SSE work.
        if hasattr(self._transport, "start_sse"):
            if hasattr(self._transport, "handler_id"):
                self._transport.handler_id = self._handler_id
            try:
                await self._transport.start_sse()
            except (OSError, TimeoutError, asyncio.TimeoutError) as exc:
                self._logger.error(str(exc))
                raise

        dispatch_task = asyncio.create_task(self._dispatch_loop())
        shutdown_task = asyncio.create_task(self._shutdown.wait())
        try:
            done, _pending = await asyncio.wait(
                {dispatch_task, shutdown_task},
                return_when=asyncio.FIRST_COMPLETED,
            )
        finally:
            for task in {dispatch_task, shutdown_task}:
                if not task.done():
                    task.cancel()
                    try:
                        await task
                    except asyncio.CancelledError:
                        pass

        if shutdown_task not in done:
            raise TransportDisconnected("daemon disconnected")

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
        parser.add_argument(
            "--retry-initial-backoff",
            type=float,
            default=None,
            help="Initial retry backoff in seconds (default: 1.0).",
        )
        parser.add_argument(
            "--retry-max-backoff",
            type=float,
            default=None,
            help="Maximum retry backoff in seconds (default: 30.0).",
        )
        parser.add_argument(
            "--retry-jitter-ratio",
            type=float,
            default=None,
            help="Random jitter as a ratio of the current backoff (default: 0.2).",
        )
        parser.add_argument(
            "--circuit-failure-threshold",
            type=int,
            default=None,
            help="Consecutive failures before the circuit opens (default: 5).",
        )
        parser.add_argument(
            "--circuit-cooling-off-seconds",
            type=float,
            default=None,
            help="Seconds the circuit stays open before probing (default: 60.0).",
        )
        parser.add_argument(
            "--degraded-log-interval",
            type=float,
            default=None,
            help="Minimum seconds between degraded status logs (default: 60.0; 0 disables).",
        )
        parser.add_argument(
            "--log-level",
            default=None,
            help="Log level (debug, info, warn, error). Defaults to the constructor argument or $PACTO_LOG_LEVEL or info.",
        )
        return parser.parse_args(argv)

    async def _dispatch_loop(self) -> None:
        try:
            async for notification in self._client.notifications():
                if isinstance(notification, AgentEventParams):
                    await self._handle_event(notification)
                elif isinstance(notification, AgentStatusParams):
                    await self._handle_status(notification)
                elif isinstance(notification, AgentRateLimitedParams):
                    await self._handle_rate_limited(notification)
        except asyncio.CancelledError:
            pass

    async def _handle_event(self, event: AgentEventParams) -> None:
        self._logger.debug(
            "incoming agent.event: "
            + json.dumps(event.model_dump(mode="json", exclude_none=True))
        )
        parsed = parse_command(event.content)
        command: str | None = None

        if parsed is None:
            if self._default_handler is None:
                self._logger.info(f"ignoring malformed event {event.event_id}")
                await self._client.handler_response(
                    action="ignore", event_id=event.event_id
                )
                return
            self._logger.info(
                f"routing non-command event {event.event_id} to default handler"
            )
            handler = self._default_handler
        else:
            command = parsed["command"]
            handler = self._commands.get(command) or self._default_handler
            self._logger.info(f"dispatching command {command} for event {event.event_id}")

        if handler is None:
            await self._client.handler_response(
                action="ignore", event_id=event.event_id
            )
            return

        try:
            result = handler(event, self)
            if inspect.isawaitable(result):
                result = await result
        except Exception as exc:
            label = command if command else "default"
            self._logger.debug(traceback.format_exc())
            self._logger.error(f"handler error for {label}: {exc}")
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
            self._logger.warn(f"handler returned invalid response: {result!r}")
            await self._client.handler_response(
                action="ignore", event_id=event.event_id
            )
            return

        self._logger.debug("outgoing handler.response: " + json.dumps(result))
        self._logger.info(
            f"handler response {event.event_id}: action={result['action']}"
        )
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
            self._logger.info(f"daemon status: {status.state}")

    async def _handle_rate_limited(self, params: AgentRateLimitedParams) -> None:
        self._logger.info(f"agent rate limited: {params.window_seconds}s window")
        if self._rate_limited_handler is not None:
            result = self._rate_limited_handler(params, self)
            if inspect.isawaitable(result):
                await result

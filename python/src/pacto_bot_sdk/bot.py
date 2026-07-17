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

    By default, the bot does not announce itself when joining a Squad. Set
    ``hello_message`` to enable a short automatic message on
    ``mls_welcome_received`` events. The bot must also have the
    ``SendGroupMessages`` capability. For full control, use
    ``@bot.on_squad_join`` to provide a custom handler.

    Set ``version`` to a bot-specific version string and the bot will
    automatically respond to ``/version`` and ``/info`` with that string.
    When ``version`` is omitted, the commands still work and return
    ``"unknown"``.
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
        auto_acknowledge: bool = True,
        retry_initial_backoff: float = 1.0,
        retry_max_backoff: float = 30.0,
        retry_jitter_ratio: float = 0.2,
        circuit_failure_threshold: int = 5,
        circuit_cooling_off_seconds: float = 60.0,
        degraded_log_interval: float = 60.0,
        log_level: str | None = None,
        hello_message: str | None = None,
        version: str | None = None,
    ) -> None:
        self.bot_id = bot_id
        self.event_types = list(event_types or ["dm_received"])
        self.capabilities = list(capabilities or ["ReadMessages", "SendMessages"])
        self.reply_on_error = reply_on_error
        self.error_message = error_message
        self.auto_acknowledge = auto_acknowledge
        self._hello_message = hello_message
        self._version = version if version is not None else "unknown"

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
        self._event_handlers: dict[str, CommandHandler] = {}
        self._hears: dict[str, CommandHandler] = {}
        self._default_handler: CommandHandler | None = None
        self._status_handler: StatusHandler | None = None
        self._rate_limited_handler: RateLimitedHandler | None = None

        self._commands["version"] = self._version_handler
        self._commands["info"] = self._version_handler

        if self._hello_message is not None:
            if "SendGroupMessages" in self.capabilities:
                if "mls_welcome_received" not in self.event_types:
                    self.event_types.append("mls_welcome_received")
                self._event_handlers.setdefault(
                    "mls_welcome_received", self._default_squad_join_handler
                )
            else:
                self._logger.warn(
                    "hello_message is set but SendGroupMessages capability is missing; "
                    "squad join auto-hello is disabled"
                )

        self._own_pubkeys: dict[str, str] | None = None
        self._decorator_state_lock = asyncio.Lock()
        self._throttle_state: dict[str, float] = {}
        self._lock_state: dict[str, asyncio.Lock] = {}
        self._lock_waiters: dict[str, int] = {}
        self._seen_unknown_notifications: set[str] = set()

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

    async def _version_handler(
        self, event: AgentEventParams, bot: "Bot"
    ) -> dict[str, Any]:
        """Return the configured version string for a ``/version`` command."""
        return self.reply(event, self._version)

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

    def event(self, type: str) -> Callable[[CommandHandler], CommandHandler]:
        """Register an async callback for ``agent.event`` notifications of *type*."""

        def decorator(handler: CommandHandler) -> CommandHandler:
            self._event_handlers[type] = handler
            return handler

        return decorator

    def dm(self, handler: CommandHandler) -> CommandHandler:
        """Shorthand for ``@bot.event(\"dm_received\")``."""
        self._event_handlers["dm_received"] = handler
        return handler

    def on_squad_join(self, handler: CommandHandler) -> CommandHandler:
        """Register a callback for MLS squad join (welcome) events.

        This overrides the built-in auto-hello message. Registering this
        decorator also adds ``mls_welcome_received`` to the handler's subscribed
        event types so the daemon delivers welcome events.
        """
        if "mls_welcome_received" not in self.event_types:
            self.event_types.append("mls_welcome_received")
        self._event_handlers["mls_welcome_received"] = handler
        return handler

    def hears(self, token: str) -> Callable[[CommandHandler], CommandHandler]:
        """Register an async callback for messages whose first token matches *token*."""

        def decorator(handler: CommandHandler) -> CommandHandler:
            self._hears[token] = handler
            return handler

        return decorator

    def throttle(
        self,
        key: Callable[[AgentEventParams], str],
        window_seconds: float,
        *,
        max_entries: int = 4096,
    ) -> Callable[[CommandHandler], CommandHandler]:
        """Skip repeated handler calls within *window_seconds* for the same key.

        The key function is called with the incoming event to compute a
        throttle bucket. The throttle window is tracked in-memory per ``Bot``
        instance. When a call is throttled, the wrapper returns ``None`` and
        the auto-acknowledge machinery (if enabled) emits
        ``handler_response(action=\"ignore\")``.

        Place throttle *under* routing decorators (``@command``, ``@event``,
        ``@dm``, ``@hears``, ``@default``) so the routing decorator registers
        the wrapped handler.
        """

        def decorator(handler: CommandHandler) -> CommandHandler:
            async def wrapper(event: AgentEventParams, bot: "Bot") -> Any:
                try:
                    bucket = key(event)
                except Exception as exc:
                    self._logger.error(f"throttle key function failed: {exc}")
                    return await self._await_handler(handler, event)

                now = asyncio.get_running_loop().time()
                async with self._decorator_state_lock:
                    last = self._throttle_state.get(bucket, 0.0)
                    if now - last < window_seconds:
                        self._logger.debug(f"throttle skip {bucket}")
                        return None
                    self._throttle_state[bucket] = now
                    # Bound memory growth: prune oldest entries when over limit.
                    if len(self._throttle_state) > max_entries:
                        oldest = min(
                            self._throttle_state.items(),
                            key=lambda item: item[1],
                        )[0]
                        self._throttle_state.pop(oldest, None)
                return await self._await_handler(handler, event)

            return wrapper

        return decorator

    def lock(
        self,
        name: str,
        *,
        on_conflict: str = "queue",
        max_waiters: int | None = None,
    ) -> Callable[[CommandHandler], CommandHandler]:
        """Serialize or skip overlapping handler calls named *name*.

        * ``on_conflict=\"queue\"`` (default) queues the call until the lock is
          released.
        * ``on_conflict=\"skip\"`` returns ``None`` immediately when the lock is
          already held.

        If *max_waiters* is set, at most that many tasks are allowed to wait in
        the queue behind the one currently holding the lock; additional calls
        are skipped.

        ``asyncio.Lock`` is not reentrant: a handler that calls itself through
        the same lock will deadlock. Keep critical sections short and do not
        invoke the same decorated handler recursively.

        Place lock *under* routing decorators (``@command``, ``@event``,
        ``@dm``, ``@hears``, ``@default``) so the routing decorator registers
        the wrapped handler.
        """
        if on_conflict not in {"queue", "skip"}:
            raise ValueError("lock on_conflict must be 'queue' or 'skip'")

        def decorator(handler: CommandHandler) -> CommandHandler:
            async def wrapper(event: AgentEventParams, bot: "Bot") -> Any:
                async with self._decorator_state_lock:
                    lock = self._lock_state.get(name)
                    if lock is None:
                        lock = asyncio.Lock()
                        self._lock_state[name] = lock

                if on_conflict == "skip":
                    if lock.locked():
                        self._logger.debug(f"lock skip {name}")
                        return None
                    await lock.acquire()
                    try:
                        return await self._await_handler(handler, event)
                    finally:
                        lock.release()

                # on_conflict == "queue"
                is_waiter = False
                if max_waiters is not None:
                    # Limit the number of tasks waiting behind the lock holder.
                    async with self._decorator_state_lock:
                        if lock.locked():
                            active = self._lock_waiters.get(name, 0)
                            if active >= max_waiters:
                                self._logger.debug(f"lock max waiters {name}")
                                return None
                            self._lock_waiters[name] = active + 1
                            is_waiter = True
                try:
                    async with lock:
                        return await self._await_handler(handler, event)
                finally:
                    if is_waiter:
                        async with self._decorator_state_lock:
                            self._lock_waiters[name] = max(
                                self._lock_waiters.get(name, 1) - 1, 0
                            )

            return wrapper

        return decorator

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

    @property
    def own_pubkey(self) -> str | None:
        """The bot's Nostr public key (npub) as reported by the daemon.

        Populated after a successful ``handler.register`` or ``handler.reconnect``.
        """
        if self._own_pubkeys is None:
            return None
        return self._own_pubkeys.get(self.bot_id)

    def ignore(self, event: AgentEventParams) -> dict[str, Any]:
        """Return a terminal ``handler_response(action=\"ignore\")`` dict."""
        return {"event_id": event.event_id, "action": "ignore"}

    def reply(self, event: AgentEventParams, content: str) -> dict[str, Any]:
        """Return a terminal ``handler_response(action=\"reply\")`` dict.

        ``content`` must be a ``str`` with a UTF-8 encoded length of at most
        8192 bytes. Callers are responsible for sanitizing any user-derived
        content before passing it here.
        """
        if not isinstance(content, str):
            raise ValueError("reply content must be a string")
        if len(content.encode("utf-8")) > 8192:
            raise ValueError("reply content exceeds 8192 bytes")
        return {"event_id": event.event_id, "action": "reply", "content": content}

    async def send_group_message(self, group_id: str, content: str) -> str:
        """Send an encrypted MLS group message as this bot."""
        return await self._client.agent_send_group_message(
            bot_id=self.bot_id,
            group_id=group_id,
            content=content,
        )

    async def _default_squad_join_handler(
        self, event: AgentEventParams, bot: "Bot"
    ) -> dict[str, Any] | None:
        """Send the configured auto-hello message when joining a Squad."""
        if not event.chat_id:
            self._logger.warn(
                "mls_welcome_received event has no chat_id; cannot send hello"
            )
            return self.ignore(event)
        try:
            content = self._hello_message.format(bot_id=self.bot_id)
        except Exception as exc:
            self._logger.error(f"failed to format hello_message: {exc}")
            return self.ignore(event)
        try:
            await self.send_group_message(event.chat_id, content)
        except PactoClientError as exc:
            self._logger.error(f"failed to send squad hello: {exc}")
        return self.ignore(event)

    async def is_squad_member(self, group_id: str, member_pubkey: str) -> bool:
        """Check whether *member_pubkey* is a member of the Squad *group_id*."""
        response = await self._client.agent_is_squad_member(
            bot_id=self.bot_id,
            group_id=group_id,
            member_pubkey=member_pubkey,
        )
        return response.is_member

    async def exit_squad(self, group_id: str) -> str:
        """Exit the Squad *group_id* by publishing a self-removal MLS proposal.

        Returns the hex event id of the published kind:445 evolution event.
        """
        response = await self._client.agent_exit_mls_group(
            bot_id=self.bot_id,
            group_id=group_id,
        )
        return response.event_id

    async def create_mls_group(
        self,
        group_name: str,
        recipient: str,
    ) -> str:
        """Create a new MLS group and invite the recipient as this bot."""
        response = await self._client.agent_create_mls_group(
            bot_id=self.bot_id,
            group_name=group_name,
            recipient=recipient,
        )
        return response.wire_id

    async def invite_to_mls_group(
        self,
        group_name: str,
        recipient: str,
    ) -> str:
        """Invite a recipient to an existing MLS group as this bot."""
        response = await self._client.agent_invite_to_mls_group(
            bot_id=self.bot_id,
            group_name=group_name,
            recipient=recipient,
        )
        return response.wire_id

    # -----------------------------------------------------------------------
    # Internal handler invocation
    # -----------------------------------------------------------------------

    async def _await_handler(self, handler: CommandHandler, event: AgentEventParams) -> Any:
        """Call a sync or async handler and return its result."""
        result = handler(event, self)
        if inspect.isawaitable(result):
            result = await result
        return result

    async def _invoke_handler(
        self, event: AgentEventParams, handler: CommandHandler, label: str
    ) -> None:
        """Invoke a decorated handler and emit a terminal ``handler.response``.

        When ``auto_acknowledge`` is enabled, ``None`` returns become
        ``action=\"ignore\"`` responses. Exceptions are logged and answered with
        a friendly reply or ignore based on ``reply_on_error``. Invalid dict
        returns are logged and treated as ignore.
        """
        try:
            result = await self._await_handler(handler, event)
        except Exception as exc:
            self._logger.debug(traceback.format_exc())
            self._logger.error(f"handler error for {label}: {exc}")
            if self.reply_on_error:
                await self._send_handler_response(self.reply(event, self.error_message))
            else:
                await self._send_handler_response(self.ignore(event))
            return

        if result is None:
            if self.auto_acknowledge:
                await self._send_handler_response(self.ignore(event))
            return

        if (
            not isinstance(result, dict)
            or "event_id" not in result
            or "action" not in result
        ):
            self._logger.warn(f"handler returned invalid response: {result!r}")
            await self._send_handler_response(self.ignore(event))
            return

        await self._send_handler_response(result)

    async def _send_handler_response(self, result: dict[str, Any]) -> None:
        """Send a terminal ``handler.response`` frame and log it."""
        self._logger.debug("outgoing handler.response: " + json.dumps(result))
        self._logger.info(
            f"handler response {result['event_id']}: action={result['action']}"
        )
        content = result.get("content")
        kwargs: dict[str, Any] = {
            "action": result["action"],
            "event_id": result["event_id"],
        }
        if content is not None:
            kwargs["content"] = content
        await self._client.handler_response(**kwargs)

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

        # Daemon error codes that mean the stored registration is no longer
        # valid and we should fall back to a fresh handler.register.
        _STALE_REGISTRATION_CODES = frozenset({-32001, -32008})

        try:
            if self._handler_id and self._reconnect_token:
                try:
                    result = await self._client.handler_reconnect(
                        handler_id=self._handler_id,
                        reconnect_token=self._reconnect_token,
                    )
                except PactoClientError as exc:
                    if getattr(exc, "code", None) not in _STALE_REGISTRATION_CODES:
                        raise
                    self._logger.warn(
                        f"reconnect token rejected ({exc}); falling back to fresh registration"
                    )
                    self._handler_id = None
                    self._reconnect_token = None
                    result = await self._client.handler_register(
                        bot_ids=[self.bot_id],
                        event_types=self.event_types,
                        capabilities=self.capabilities,
                    )
                    self._reconnect_token = result.reconnect_token
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
        self._own_pubkeys = result.own_pubkeys
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
                else:
                    note_type = type(notification).__name__
                    if note_type not in self._seen_unknown_notifications:
                        self._seen_unknown_notifications.add(note_type)
                        self._logger.warn(
                            f"unknown notification type: {note_type}; ignoring"
                        )
        except asyncio.CancelledError:
            pass

    async def _handle_event(self, event: AgentEventParams) -> None:
        self._logger.debug(
            "incoming agent.event: "
            + json.dumps(event.model_dump(mode="json", exclude_none=True))
        )

        handler: CommandHandler | None = None
        label: str | None = None

        if event.type in self._event_handlers:
            handler = self._event_handlers[event.type]
            label = f"event:{event.type}"
        else:
            first_token = ""
            stripped = event.content.strip()
            if stripped:
                first_token = stripped.split(maxsplit=1)[0]
            if first_token in self._hears:
                handler = self._hears[first_token]
                label = f"hears:{first_token}"
            else:
                parsed = parse_command(event.content)
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
                    label = "default"
                else:
                    command = parsed["command"]
                    handler = self._commands.get(command) or self._default_handler
                    label = f"command:{command}"

        if handler is None:
            await self._client.handler_response(
                action="ignore", event_id=event.event_id
            )
            return

        await self._invoke_handler(event, handler, label)

    async def _handle_status(self, status: AgentStatusParams) -> None:
        if self._status_handler is not None:
            result = self._status_handler(status, self)
            if inspect.isawaitable(result):
                await result
        else:
            self._logger.info(f"daemon status: {status.state}")

    async def _handle_rate_limited(self, params: AgentRateLimitedParams) -> None:
        if self._rate_limited_handler is not None:
            try:
                result = self._rate_limited_handler(params, self)
                if inspect.isawaitable(result):
                    await result
            except Exception as exc:
                self._logger.debug(traceback.format_exc())
                self._logger.error(f"rate limited handler error: {exc}")
        else:
            self._logger.info(f"agent rate limited: {params.window_seconds}s window")

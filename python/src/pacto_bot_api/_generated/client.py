# Generated from schemas/jsonrpc.json — do not edit manually.
# Run `cargo xtask codegen` to regenerate.

from __future__ import annotations
import asyncio
import json
import uuid
from typing import Any

from . import models
from pydantic import BaseModel

"""Low-level async JSON-RPC client generated from schemas/jsonrpc.json."""

class PactoClientError(Exception):
    """Error returned by the daemon for a JSON-RPC request."""

class PactoClient:
    """Transport-agnostic async client for the pacto-bot-api daemon."""

    def __init__(self, transport: Any) -> None:
        self.transport = transport
        self._inflight: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._notify_queue: asyncio.Queue[BaseModel] = asyncio.Queue()
        self._read_task: asyncio.Task[None] | None = None
        self._closed = False

    async def connect(self) -> None:
        """Connect the transport and start the background read loop."""
        await self.transport.connect()
        self._read_task = asyncio.create_task(self._read_loop())

    async def close(self) -> None:
        """Stop the read loop and close the transport."""
        self._closed = True
        if self._read_task is not None:
            self._read_task.cancel()
            try:
                await self._read_task
            except asyncio.CancelledError:
                pass
        await self.transport.close()

    async def _request(
        self, method: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        """Send a JSON-RPC request and await its correlated response."""
        request_id = str(uuid.uuid4())
        frame = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
        future: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        self._inflight[request_id] = future
        try:
            immediate = await self.transport.write_frame(frame)
            if immediate is not None:
                self._resolve(request_id, immediate)
            response = await future
            if "error" in response:
                error = response["error"]
                raise PactoClientError(
                    error.get("message", str(error))
                ) from None
            return response
        finally:
            self._inflight.pop(request_id, None)

    def _resolve(self, request_id: str, response: dict[str, Any]) -> None:
        future = self._inflight.pop(request_id, None)
        if future is not None and not future.done():
            future.set_result(response)

    async def _read_loop(self) -> None:
        while not self._closed:
            try:
                line = await self.transport.readline()
            except asyncio.CancelledError:
                break
            except Exception:  # pragma: no cover - defensive
                continue
            if not line:
                break
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                continue
            await self._dispatch_frame(frame)

    async def _dispatch_frame(self, frame: dict[str, Any]) -> None:
        if "id" in frame:
            self._resolve(str(frame['id']), frame)
            return
        method = frame.get('method')
        params = frame.get('params', {})
        if method == 'agent.event':
            await self._notify_queue.put(models.AgentEventParams.model_validate(params))
        elif method == 'agent.status':
            await self._notify_queue.put(models.AgentStatusParams.model_validate(params))

    async def notifications(self) -> Any:
        """Async iterator over incoming daemon notifications."""
        while not self._closed:
            yield await self._notify_queue.get()

    async def agent_error(self, bot_id: str, message: str, code: str | None = None, data: Any | None = None) -> None:
        """
        Send JSON-RPC notification `agent.error`.

        Report an error encountered by a handler.

        Example:

            >>> await client.agent_error(...)

        jsonrpc_method: ``"agent.error"``
        """
        params = models.AgentErrorParams(bot_id=bot_id, code=code, data=data, message=message)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        frame = {
            "jsonrpc": "2.0",
            "method": "agent.error",
            "params": params_dict,
        }
        await self.transport.write_frame(frame)

    async def agent_metrics(self) -> AgentMetricsResult:
        """
        Call JSON-RPC method `agent.metrics`.

        Return a machine-readable health and metrics snapshot.

        Example:

            >>> result = await client.agent_metrics(...)
            >>> isinstance(result, AgentMetricsResult)

        jsonrpc_method: ``"agent.metrics"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("agent.metrics", params_dict)
        result = response.get('result')
        return result

    async def agent_send_dm(self, bot_id: str, content: str, recipient: str, reply_to: str | None = None) -> AgentSendDmResult:
        """
        Call JSON-RPC method `agent.send_dm`.

        Send a direct message as the specified bot.

        Example:

            >>> result = await client.agent_send_dm(...)
            >>> isinstance(result, AgentSendDmResult)

        jsonrpc_method: ``"agent.send_dm"``
        """
        params = models.AgentSendDmParams(bot_id=bot_id, content=content, recipient=recipient, reply_to=reply_to)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.send_dm", params_dict)
        result = response.get('result')
        return result

    async def agent_set_profile(self, bot_id: str, about: str | None = None, name: str | None = None, picture: str | None = None) -> AgentSetProfileResult:
        """
        Call JSON-RPC method `agent.set_profile`.

        Update the bot's Nostr kind:0 profile.

        Example:

            >>> result = await client.agent_set_profile(...)
            >>> isinstance(result, AgentSetProfileResult)

        jsonrpc_method: ``"agent.set_profile"``
        """
        params = models.AgentSetProfileParams(about=about, bot_id=bot_id, name=name, picture=picture)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.set_profile", params_dict)
        result = response.get('result')
        return result

    async def agent_version(self) -> AgentVersionResult:
        """
        Call JSON-RPC method `agent.version`.

        Return the daemon version and git commit hash.

        Example:

            >>> result = await client.agent_version(...)
            >>> isinstance(result, AgentVersionResult)

        jsonrpc_method: ``"agent.version"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("agent.version", params_dict)
        result = response.get('result')
        return result

    async def handler_register(self, bot_ids: list[str], capabilities: list[str], event_types: list[str]) -> HandlerRegisterResult:
        """
        Call JSON-RPC method `handler.register`.

        Register a handler connection for event delivery.

        Example:

            >>> result = await client.handler_register(...)
            >>> isinstance(result, HandlerRegisterResult)

        jsonrpc_method: ``"handler.register"``
        """
        params = models.HandlerRegisterParams(bot_ids=bot_ids, capabilities=capabilities, event_types=event_types)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("handler.register", params_dict)
        result = response.get('result')
        return models.HandlerRegisterResult.model_validate(result)

    async def handler_response(self, action: str, event_id: str, content: str | None = None) -> None:
        """
        Send JSON-RPC notification `handler.response`.

        Handler reply to a delivered agent.event.

        Example:

            >>> await client.handler_response(...)

        jsonrpc_method: ``"handler.response"``
        """
        params = models.HandlerResponseParams(action=action, content=content, event_id=event_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        frame = {
            "jsonrpc": "2.0",
            "method": "handler.response",
            "params": params_dict,
        }
        await self.transport.write_frame(frame)

    async def handler_unregister(self) -> HandlerUnregisterResult:
        """
        Call JSON-RPC method `handler.unregister`.

        Remove a handler from the routing table.

        Example:

            >>> result = await client.handler_unregister(...)
            >>> isinstance(result, HandlerUnregisterResult)

        jsonrpc_method: ``"handler.unregister"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("handler.unregister", params_dict)
        result = response.get('result')
        return models.HandlerUnregisterResult.model_validate(result)

__all__ = ['PactoClient', 'PactoClientError']

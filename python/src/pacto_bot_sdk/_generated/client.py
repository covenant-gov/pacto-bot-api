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
    """Transport-agnostic async client for the Pacto daemon."""

    def __init__(self, transport: Any) -> None:
        self.transport = transport
        self._inflight: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._notify_queue: asyncio.Queue[BaseModel | None] = asyncio.Queue()
        self._read_task: asyncio.Task[None] | None = None
        self._closed = False

    async def connect(self) -> None:
        """Connect the transport and start the background read loop."""
        await self.transport.connect()
        self._read_task = asyncio.create_task(self._read_loop())

    async def close(self) -> None:
        """Stop the read loop and close the transport."""
        self._closed = True
        await self._notify_queue.put(None)
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
            except Exception:  # pragma: no cover - transport disconnect
                break
            if not line:
                break
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                continue
            await self._dispatch_frame(frame)
        await self._notify_queue.put(None)
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
            notification = await self._notify_queue.get()
            if notification is None:
                break
            yield notification

    async def admin_send_test_dm(self, bot_id: str, content: str, recipient: str) -> models.AdminSendTestDmResult:
        """
        Call JSON-RPC method `admin.send_test_dm`.

        Send a test DM as the specified bot (admin-only).

        Example:

            >>> result = await client.admin_send_test_dm(...)
            >>> isinstance(result, AdminSendTestDmResult)

        jsonrpc_method: ``"admin.send_test_dm"``
        """
        params = models.AdminSendTestDmParams(bot_id=bot_id, content=content, recipient=recipient)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("admin.send_test_dm", params_dict)
        result = response.get('result')
        return models.AdminSendTestDmResult.model_validate(result)

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

    async def agent_list_handlers(self) -> models.AgentListHandlersResult:
        """
        Call JSON-RPC method `agent.list_handlers`.

        Return the daemon's handler routing table (admin-only).

        Example:

            >>> result = await client.agent_list_handlers(...)
            >>> isinstance(result, AgentListHandlersResult)

        jsonrpc_method: ``"agent.list_handlers"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("agent.list_handlers", params_dict)
        result = response.get('result')
        return models.AgentListHandlersResult.model_validate(result)

    async def agent_metrics(self) -> models.AgentMetricsResult:
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

    async def agent_publish_key_package(self, bot_id: str) -> models.AgentPublishKeyPackageResult:
        """
        Call JSON-RPC method `agent.publish_key_package`.

        Publish a Nostr MLS KeyPackage event (kind:443) for the specified bot.

        Example:

            >>> result = await client.agent_publish_key_package(...)
            >>> isinstance(result, AgentPublishKeyPackageResult)

        jsonrpc_method: ``"agent.publish_key_package"``
        """
        params = models.AgentPublishKeyPackageParams(bot_id=bot_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.publish_key_package", params_dict)
        result = response.get('result')
        return result

    async def agent_send_dm(self, bot_id: str, content: str, recipient: str, reply_to: str | None = None) -> models.AgentSendDmResult:
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

    async def agent_send_group_message(self, bot_id: str, content: str, group_id: str) -> models.AgentSendGroupMessageResult:
        """
        Call JSON-RPC method `agent.send_group_message`.

        Send an encrypted MLS group message as the specified bot.

        Example:

            >>> result = await client.agent_send_group_message(...)
            >>> isinstance(result, AgentSendGroupMessageResult)

        jsonrpc_method: ``"agent.send_group_message"``
        """
        params = models.AgentSendGroupMessageParams(bot_id=bot_id, content=content, group_id=group_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.send_group_message", params_dict)
        result = response.get('result')
        return result

    async def agent_set_profile(self, bot_id: str, about: str | None = None, name: str | None = None, picture: str | None = None) -> models.AgentSetProfileResult:
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

    async def agent_unregister_handler(self, handler_id: str) -> models.AgentUnregisterHandlerResult:
        """
        Call JSON-RPC method `agent.unregister_handler`.

        Forcibly remove a handler from the routing table. The caller must be the target handler itself or have the Admin capability.

        Example:

            >>> result = await client.agent_unregister_handler(...)
            >>> isinstance(result, AgentUnregisterHandlerResult)

        jsonrpc_method: ``"agent.unregister_handler"``
        """
        params = models.AgentUnregisterHandlerParams(handler_id=handler_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.unregister_handler", params_dict)
        result = response.get('result')
        return models.AgentUnregisterHandlerResult.model_validate(result)

    async def agent_version(self) -> models.AgentVersionResult:
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

    async def handler_reconnect(self, handler_id: str, reconnect_token: str) -> models.HandlerReconnectResult:
        """
        Call JSON-RPC method `handler.reconnect`.

        Reconnect a previously registered handler using its secret reconnect token.

        Example:

            >>> result = await client.handler_reconnect(...)
            >>> isinstance(result, HandlerReconnectResult)

        jsonrpc_method: ``"handler.reconnect"``
        """
        params = models.HandlerReconnectParams(handler_id=handler_id, reconnect_token=reconnect_token)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("handler.reconnect", params_dict)
        result = response.get('result')
        return models.HandlerReconnectResult.model_validate(result)

    async def handler_register(self, bot_ids: list[str], capabilities: list[str], event_types: list[str]) -> models.HandlerRegisterResult:
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

    async def handler_unregister(self) -> models.HandlerUnregisterResult:
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

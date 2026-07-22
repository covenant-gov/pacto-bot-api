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

# Sentinel used to indicate 'use the client's default timeout' in method signatures.
_DEFAULT_TIMEOUT: Any = object()

class PactoClientError(Exception):
    """Error returned by the daemon for a JSON-RPC request."""

    def __init__(self, message: str, code: int | None = None) -> None:
        super().__init__(message)
        self.code = code

class PactoClient:
    """Transport-agnostic async client for the Pacto daemon."""

    def __init__(self, transport: Any, timeout: float | None = None) -> None:
        self.transport = transport
        self._default_timeout: float | None = timeout if timeout is not None else 30.0
        self._inflight: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._notify_queue: asyncio.Queue[BaseModel | None] = asyncio.Queue(maxsize=100)
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
        self, method: str, params: dict[str, Any], timeout: float | None = None
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
            if timeout is _DEFAULT_TIMEOUT:
                effective_timeout = self._default_timeout
            else:
                effective_timeout = timeout
            if effective_timeout is not None:
                response = await asyncio.wait_for(future, timeout=effective_timeout)
            else:
                response = await future
            if "error" in response:
                error = response["error"]
                raise PactoClientError(
                    error.get("message", str(error)),
                    code=error.get("code"),
                ) from None
            return response
        except asyncio.TimeoutError as exc:
            raise PactoClientError(
                f"Request timed out after {effective_timeout} seconds"
            ) from exc
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
        elif method == 'agent.rate_limited':
            await self._notify_queue.put(models.AgentRateLimitedParams.model_validate(params))
        elif method == 'agent.status':
            await self._notify_queue.put(models.AgentStatusParams.model_validate(params))

    async def notifications(self) -> Any:
        """Async iterator over incoming daemon notifications."""
        while not self._closed:
            notification = await self._notify_queue.get()
            if notification is None:
                break
            yield notification

    async def admin_create_mls_group(self, bot_id: str, group_name: str, recipient: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AdminCreateMlsGroupResponse:
        """
        Call JSON-RPC method `admin.create_mls_group`.

        Create a new MLS group and invite the recipient (admin-only).

        Example:

            >>> result = await client.admin_create_mls_group(...)
            >>> isinstance(result, AdminCreateMlsGroupResponse)

        jsonrpc_method: ``"admin.create_mls_group"``
        """
        params = models.AdminCreateMlsGroupParams(bot_id=bot_id, group_name=group_name, recipient=recipient)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("admin.create_mls_group", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AdminCreateMlsGroupResponse.model_validate(result)

    async def admin_invite_to_mls_group(self, bot_id: str, group_name: str, recipient: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AdminInviteToMlsGroupResponse:
        """
        Call JSON-RPC method `admin.invite_to_mls_group`.

        Invite a recipient to an existing MLS group (admin-only).

        Example:

            >>> result = await client.admin_invite_to_mls_group(...)
            >>> isinstance(result, AdminInviteToMlsGroupResponse)

        jsonrpc_method: ``"admin.invite_to_mls_group"``
        """
        params = models.AdminInviteToMlsGroupParams(bot_id=bot_id, group_name=group_name, recipient=recipient)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("admin.invite_to_mls_group", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AdminInviteToMlsGroupResponse.model_validate(result)

    async def admin_send_test_dm(self, bot_id: str, content: str, recipient: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AdminSendTestDmResponse:
        """
        Call JSON-RPC method `admin.send_test_dm`.

        Send a test DM as the specified bot (admin-only).

        Example:

            >>> result = await client.admin_send_test_dm(...)
            >>> isinstance(result, AdminSendTestDmResponse)

        jsonrpc_method: ``"admin.send_test_dm"``
        """
        params = models.AdminSendTestDmParams(bot_id=bot_id, content=content, recipient=recipient)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("admin.send_test_dm", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AdminSendTestDmResponse.model_validate(result)

    async def agent_create_mls_group(self, bot_id: str, group_name: str, recipient: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentCreateMlsGroupResponse:
        """
        Call JSON-RPC method `agent.create_mls_group`.

        Create a new MLS group and invite the recipient.

        Example:

            >>> result = await client.agent_create_mls_group(...)
            >>> isinstance(result, AgentCreateMlsGroupResponse)

        jsonrpc_method: ``"agent.create_mls_group"``
        """
        params = models.AgentCreateMlsGroupParams(bot_id=bot_id, group_name=group_name, recipient=recipient)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.create_mls_group", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AgentCreateMlsGroupResponse.model_validate(result)

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

    async def agent_exit_mls_group(self, bot_id: str, group_id: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentExitMlsGroupResponse:
        """
        Call JSON-RPC method `agent.exit_mls_group`.

        Exit a Squad by publishing a self-removal MLS proposal.

        Example:

            >>> result = await client.agent_exit_mls_group(...)
            >>> isinstance(result, AgentExitMlsGroupResponse)

        jsonrpc_method: ``"agent.exit_mls_group"``
        """
        params = models.AgentExitMlsGroupParams(bot_id=bot_id, group_id=group_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.exit_mls_group", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AgentExitMlsGroupResponse.model_validate(result)

    async def agent_invite_to_mls_group(self, bot_id: str, group_name: str, recipient: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentInviteToMlsGroupResponse:
        """
        Call JSON-RPC method `agent.invite_to_mls_group`.

        Invite a recipient to an existing MLS group.

        Example:

            >>> result = await client.agent_invite_to_mls_group(...)
            >>> isinstance(result, AgentInviteToMlsGroupResponse)

        jsonrpc_method: ``"agent.invite_to_mls_group"``
        """
        params = models.AgentInviteToMlsGroupParams(bot_id=bot_id, group_name=group_name, recipient=recipient)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.invite_to_mls_group", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AgentInviteToMlsGroupResponse.model_validate(result)

    async def agent_is_squad_member(self, bot_id: str, group_id: str, member_pubkey: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentIsSquadMemberResponse:
        """
        Call JSON-RPC method `agent.is_squad_member`.

        Verify whether a Nostr public key is a member of a Squad.

        Example:

            >>> result = await client.agent_is_squad_member(...)
            >>> isinstance(result, AgentIsSquadMemberResponse)

        jsonrpc_method: ``"agent.is_squad_member"``
        """
        params = models.AgentIsSquadMemberParams(bot_id=bot_id, group_id=group_id, member_pubkey=member_pubkey)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.is_squad_member", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AgentIsSquadMemberResponse.model_validate(result)

    async def agent_list_handlers(self, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentListHandlersResponse:
        """
        Call JSON-RPC method `agent.list_handlers`.

        Return the daemon's handler routing table (admin-only).

        Example:

            >>> result = await client.agent_list_handlers(...)
            >>> isinstance(result, AgentListHandlersResponse)

        jsonrpc_method: ``"agent.list_handlers"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("agent.list_handlers", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AgentListHandlersResponse.model_validate(result)

    async def agent_metrics(self, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentMetricsResponse:
        """
        Call JSON-RPC method `agent.metrics`.

        Return a machine-readable health and metrics snapshot.

        Example:

            >>> result = await client.agent_metrics(...)
            >>> isinstance(result, AgentMetricsResponse)

        jsonrpc_method: ``"agent.metrics"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("agent.metrics", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def agent_publish_key_package(self, bot_id: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentPublishKeyPackageResponse:
        """
        Call JSON-RPC method `agent.publish_key_package`.

        Publish a Nostr MLS KeyPackage event (kind:443) for the specified bot.

        Example:

            >>> result = await client.agent_publish_key_package(...)
            >>> isinstance(result, AgentPublishKeyPackageResponse)

        jsonrpc_method: ``"agent.publish_key_package"``
        """
        params = models.AgentPublishKeyPackageParams(bot_id=bot_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.publish_key_package", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def agent_send_dm(self, bot_id: str, content: str, recipient: str, reply_to: str | None = None, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentSendDmResponse:
        """
        Call JSON-RPC method `agent.send_dm`.

        Send a direct message as the specified bot.

        Example:

            >>> result = await client.agent_send_dm(...)
            >>> isinstance(result, AgentSendDmResponse)

        jsonrpc_method: ``"agent.send_dm"``
        """
        params = models.AgentSendDmParams(bot_id=bot_id, content=content, recipient=recipient, reply_to=reply_to)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.send_dm", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def agent_send_group_message(self, bot_id: str, content: str, group_id: str, pacto_virtual_bucket: str | None = None, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentSendGroupMessageResponse:
        """
        Call JSON-RPC method `agent.send_group_message`.

        Send an encrypted MLS group message as the specified bot.

        Example:

            >>> result = await client.agent_send_group_message(...)
            >>> isinstance(result, AgentSendGroupMessageResponse)

        jsonrpc_method: ``"agent.send_group_message"``
        """
        params = models.AgentSendGroupMessageParams(bot_id=bot_id, content=content, group_id=group_id, pacto_virtual_bucket=pacto_virtual_bucket)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.send_group_message", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def agent_set_profile(self, bot_id: str, about: str | None = None, name: str | None = None, picture: str | None = None, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentSetProfileResponse:
        """
        Call JSON-RPC method `agent.set_profile`.

        Update the bot's Nostr kind:0 profile.

        Example:

            >>> result = await client.agent_set_profile(...)
            >>> isinstance(result, AgentSetProfileResponse)

        jsonrpc_method: ``"agent.set_profile"``
        """
        params = models.AgentSetProfileParams(about=about, bot_id=bot_id, name=name, picture=picture)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.set_profile", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def agent_unregister_handler(self, handler_id: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentUnregisterHandlerResponse:
        """
        Call JSON-RPC method `agent.unregister_handler`.

        Forcibly remove a handler from the routing table. The caller must be the target handler itself or have the Admin capability.

        Example:

            >>> result = await client.agent_unregister_handler(...)
            >>> isinstance(result, AgentUnregisterHandlerResponse)

        jsonrpc_method: ``"agent.unregister_handler"``
        """
        params = models.AgentUnregisterHandlerParams(handler_id=handler_id)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("agent.unregister_handler", params_dict, timeout=timeout)
        result = response.get('result')
        return models.AgentUnregisterHandlerResponse.model_validate(result)

    async def agent_version(self, timeout: float | None = _DEFAULT_TIMEOUT) -> models.AgentVersionResponse:
        """
        Call JSON-RPC method `agent.version`.

        Return the daemon version and git commit hash.

        Example:

            >>> result = await client.agent_version(...)
            >>> isinstance(result, AgentVersionResponse)

        jsonrpc_method: ``"agent.version"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("agent.version", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def handler_reconnect(self, handler_id: str, reconnect_token: str, timeout: float | None = _DEFAULT_TIMEOUT) -> models.HandlerReconnectResponse:
        """
        Call JSON-RPC method `handler.reconnect`.

        Reconnect a previously registered handler using its secret reconnect token.

        Example:

            >>> result = await client.handler_reconnect(...)
            >>> isinstance(result, HandlerReconnectResponse)

        jsonrpc_method: ``"handler.reconnect"``
        """
        params = models.HandlerReconnectParams(handler_id=handler_id, reconnect_token=reconnect_token)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("handler.reconnect", params_dict, timeout=timeout)
        result = response.get('result')
        return models.HandlerReconnectResponse.model_validate(result)

    async def handler_register(self, bot_ids: list[str], capabilities: list[str], event_types: list[str], timeout: float | None = _DEFAULT_TIMEOUT) -> models.HandlerRegisterResponse:
        """
        Call JSON-RPC method `handler.register`.

        Register a handler connection for event delivery.

        Example:

            >>> result = await client.handler_register(...)
            >>> isinstance(result, HandlerRegisterResponse)

        jsonrpc_method: ``"handler.register"``
        """
        params = models.HandlerRegisterParams(bot_ids=bot_ids, capabilities=capabilities, event_types=event_types)
        params_dict = params.model_dump(mode='json', exclude_none=True)
        response = await self._request("handler.register", params_dict, timeout=timeout)
        result = response.get('result')
        return models.HandlerRegisterResponse.model_validate(result)

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

    async def handler_unregister(self, timeout: float | None = _DEFAULT_TIMEOUT) -> models.HandlerUnregisterResponse:
        """
        Call JSON-RPC method `handler.unregister`.

        Remove a handler from the routing table.

        Example:

            >>> result = await client.handler_unregister(...)
            >>> isinstance(result, HandlerUnregisterResponse)

        jsonrpc_method: ``"handler.unregister"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("handler.unregister", params_dict, timeout=timeout)
        result = response.get('result')
        return models.HandlerUnregisterResponse.model_validate(result)

    async def system_health(self, timeout: float | None = _DEFAULT_TIMEOUT) -> models.SystemHealthResponse:
        """
        Call JSON-RPC method `system.health`.

        Return a machine-readable health and metrics snapshot.

        Example:

            >>> result = await client.system_health(...)
            >>> isinstance(result, SystemHealthResponse)

        jsonrpc_method: ``"system.health"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("system.health", params_dict, timeout=timeout)
        result = response.get('result')
        return result

    async def system_version(self, timeout: float | None = _DEFAULT_TIMEOUT) -> models.SystemVersionResponse:
        """
        Call JSON-RPC method `system.version`.

        Return the daemon version and git commit hash.

        Example:

            >>> result = await client.system_version(...)
            >>> isinstance(result, SystemVersionResponse)

        jsonrpc_method: ``"system.version"``
        """
        params_dict: dict[str, Any] = {}
        response = await self._request("system.version", params_dict, timeout=timeout)
        result = response.get('result')
        return result

__all__ = ['PactoClient', 'PactoClientError']

"""Shared pytest fixtures for the pacto-bot-sdk Python SDK tests."""

from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest

from pacto_bot_sdk import Bot, PactoClient


class MockTransport:
    """In-memory transport for driving ``PactoClient`` and ``Bot`` in tests."""

    name = "mock"

    def __init__(self) -> None:
        self.frames: list[dict[str, Any]] = []
        self._inbound: asyncio.Queue[str] = asyncio.Queue()
        self.connected = False
        self.closed = False
        self.handler_id: str | None = None

    async def connect(self) -> None:
        self.connected = True

    async def close(self) -> None:
        self.closed = True

    async def readline(self) -> str:
        return await self._inbound.get()

    async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
        self.frames.append(frame)
        return None

    def inject(self, frame: dict[str, Any]) -> None:
        self._inbound.put_nowait(json.dumps(frame))


@pytest.fixture
async def mock_transport() -> MockTransport:
    """Yield a connected-ready mock transport and close it after the test."""
    transport = MockTransport()
    await transport.connect()
    try:
        yield transport
    finally:
        if not transport.closed:
            await transport.close()


@pytest.fixture
async def client(mock_transport: MockTransport) -> PactoClient:
    """Yield a connected ``PactoClient`` backed by the mock transport."""
    client = PactoClient(mock_transport)
    await client.connect()
    try:
        yield client
    finally:
        await client.close()


@pytest.fixture
def bot(mock_transport: MockTransport) -> Bot:
    """Yield a ``Bot`` instance wired to the mock transport."""
    return Bot(
        bot_id="test-bot",
        transport=mock_transport,
        event_types=["dm_received"],
        capabilities=["ReadMessages", "SendMessages"],
    )

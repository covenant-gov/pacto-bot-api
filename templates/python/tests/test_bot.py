"""Unit tests for {{bot_id}} bot handler."""

from __future__ import annotations

import pytest

from {{bot_id_snake}} import bot


class FakeEvent:
    event_id = "test-event-id-123"


@pytest.mark.asyncio
async def test_default_handler_ignores():
    result = await bot._default_handler(FakeEvent(), bot)
    assert result == {"event_id": "test-event-id-123", "action": "ignore"}


{% for command in commands %}
@pytest.mark.asyncio
async def test_{{command}}_command():
    handler = bot._commands["{{command}}"]
    result = await handler(FakeEvent(), bot)
    assert result["action"] == "reply"
    assert result["event_id"] == "test-event-id-123"
    assert "{{command}}" in result["content"].lower()


{% endfor %}

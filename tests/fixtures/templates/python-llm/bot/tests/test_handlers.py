"""Unit tests for {{bot_id}} bot handler."""

from __future__ import annotations

import pytest

from {{bot_id_snake}} import bot, _command_args


class FakeEvent:
    event_id = "test-event-id-123"
    content = ""


@pytest.mark.asyncio
async def test_default_handler_ignores():
    result = await bot._default_handler(FakeEvent(), bot)
    assert result == {"event_id": "test-event-id-123", "action": "ignore"}


@pytest.mark.parametrize("content,expected", [
    ("/hello world", ["world"]),
    ("/hello", []),
    ("not a command", []),
])
def test_command_args_extracts_arguments(content, expected):
    event = FakeEvent()
    event.content = content
    assert _command_args(event) == expected


{% if test_command_handlers %}
{{test_command_handlers}}
{% endif %}

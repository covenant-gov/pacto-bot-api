"""HTTP-related tests for {{bot_id}}."""

from __future__ import annotations

{% if http %}import httpx
import pytest
import respx

from {{bot_id_snake}} import bot


class FakeEvent:
    event_id = "test-event-id-123"
    content = ""


@respx.mock
@pytest.mark.asyncio
async def test_http_command_uses_external_api():
    """Example: mock an external API and verify a handler uses it.

    Replace the route URL and the event content with the real API and command
    your bot calls. Then wire the response to the handler and assert the reply.
    """
    route = respx.get("https://api.example.com/data").mock(
        return_value=httpx.Response(200, json={"status": "ok"})
    )

    # TODO: replace with the command that triggers an HTTP call in your bot.
    handler = bot._commands["{{first_command}}"]
    event = FakeEvent()
    event.content = "/{{first_command}}"
    result = await handler(event, bot)

    assert result["action"] == "reply"
    # TODO: once the handler above calls the mocked API, also assert:
    # assert route.called
{% endif %}
{% unless http %}# This bot was scaffolded without --http. Add httpx/respx to pyproject.toml
# and implement HTTP tests here if the bot calls external APIs.
{% endunless %}

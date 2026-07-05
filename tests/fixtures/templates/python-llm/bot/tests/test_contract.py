"""Lightweight contract test for {{bot_id}}.

Loads the generated manifest.json and verifies that each declared command
contract maps to a real handler and produces a matching response. This runs
in-process without a daemon; the full end-to-end contract harness lives in
python/tests/test_examples_contract.py.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from {{bot_id_snake}} import bot


class _FakeEvent:
    def __init__(self, event_id: str, content: str) -> None:
        self.event_id = event_id
        self.content = content


def _load_manifest():
    path = Path(__file__).parent.parent / "manifest.json"
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


_MANIFEST = _load_manifest()

_EVENT_RESPONSE_PIECES = [
    piece
    for piece in _MANIFEST["contract_pieces"]
    if piece["type"] == "event_response"
]


@pytest.mark.parametrize(
    "piece",
    [pytest.param(piece, id=piece["name"]) for piece in _EVENT_RESPONSE_PIECES],
)
@pytest.mark.asyncio
async def test_event_response_piece(piece):
    inject = piece["inject_event"]
    expected = piece["expect_response"]
    content = inject["content"]

    assert content.startswith("/"), f"inject event content must be a command: {content!r}"
    command = content.lstrip("/")
    handler = bot._commands.get(command)
    assert handler is not None, f"no handler registered for /{command}"

    event = _FakeEvent(event_id=inject["event_id"], content=content)
    result = await handler(event, bot)

    for key, value in expected.items():
        assert result.get(key) == value, f"expected {key}={value!r}, got {result.get(key)!r}"

    # The scaffold ships with a placeholder; replace it with a real response.
    assert result.get("content"), "handler returned empty content"

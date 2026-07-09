"""Tests for pacto_bot_sdk.validate input validators."""

from __future__ import annotations

import pytest

from pacto_bot_sdk import validate
from pacto_bot_sdk.validate import event_id, pubkey, squad_id


# ---------------------------------------------------------------------------
# squad_id
# ---------------------------------------------------------------------------


def test_squad_id_valid():
    assert squad_id("covenant-gov") == "covenant-gov"


def test_squad_id_strips_whitespace():
    assert squad_id("  covenant-gov  ") == "covenant-gov"


def test_squad_id_rejects_empty():
    with pytest.raises(ValueError, match="squad_id must be non-empty"):
        squad_id("")
    with pytest.raises(ValueError, match="squad_id must be non-empty"):
        squad_id("   ")


def test_squad_id_rejects_non_string():
    with pytest.raises(ValueError, match="squad_id must be a string"):
        squad_id(123)  # type: ignore[arg-type]


def test_squad_id_rejects_too_long():
    with pytest.raises(ValueError, match="squad_id must be at most 256"):
        squad_id("a" * 257)


def test_squad_id_accepts_max_length():
    assert squad_id("a" * 256) == "a" * 256


# ---------------------------------------------------------------------------
# pubkey
# ---------------------------------------------------------------------------


def test_pubkey_accepts_npub1():
    key = "npub1" + "q" * 58
    assert pubkey(key) == key


def test_pubkey_accepts_hex():
    key = "0" * 64
    assert pubkey(key) == key


def test_pubkey_strips_whitespace():
    key = "0" * 64
    assert pubkey("  " + key + "  ") == key


def test_pubkey_rejects_non_string():
    with pytest.raises(ValueError, match="pubkey must be a string"):
        pubkey(123)  # type: ignore[arg-type]


def test_pubkey_rejects_npub1_with_invalid_chars():
    with pytest.raises(ValueError, match="invalid bech32"):
        pubkey("npub1abc")


def test_pubkey_rejects_npub1_with_empty_data():
    with pytest.raises(ValueError, match="npub data must be non-empty"):
        pubkey("npub1")


def test_pubkey_rejects_hex_too_short():
    with pytest.raises(ValueError, match="npub1... or 64 hex"):
        pubkey("0" * 63)


def test_pubkey_rejects_hex_too_long():
    with pytest.raises(ValueError, match="npub1... or 64 hex"):
        pubkey("0" * 65)


def test_pubkey_rejects_hex_non_hex():
    with pytest.raises(ValueError, match="lowercase hex"):
        pubkey("x" * 64)


def test_pubkey_rejects_hex_uppercase():
    with pytest.raises(ValueError, match="hex must be lowercase"):
        pubkey("A" * 64)


# ---------------------------------------------------------------------------
# event_id
# ---------------------------------------------------------------------------


def test_event_id_valid():
    assert event_id("event-123") == "event-123"


def test_event_id_strips_whitespace():
    assert event_id("  event-123  ") == "event-123"


def test_event_id_rejects_empty():
    with pytest.raises(ValueError, match="event_id must be non-empty"):
        event_id("")
    with pytest.raises(ValueError, match="event_id must be non-empty"):
        event_id("   ")


def test_event_id_rejects_non_string():
    with pytest.raises(ValueError, match="event_id must be a string"):
        event_id(123)  # type: ignore[arg-type]


def test_event_id_rejects_too_long():
    with pytest.raises(ValueError, match="event_id must be at most 128"):
        event_id("a" * 129)


def test_event_id_accepts_max_length():
    assert event_id("a" * 128) == "a" * 128


def test_validate_module_exports():
    assert validate.squad_id is squad_id
    assert validate.pubkey is pubkey
    assert validate.event_id is event_id

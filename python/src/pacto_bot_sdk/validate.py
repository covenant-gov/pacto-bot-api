"""Input validation helpers for the pacto-bot-sdk Python SDK."""

from __future__ import annotations

import re


def _trimmed(value: str, *, max_length: int, label: str) -> str:
    """Return a non-empty, trimmed string bounded by ``max_length``."""
    if not isinstance(value, str):
        raise ValueError(f"{label} must be a string")
    stripped = value.strip()
    if not stripped:
        raise ValueError(f"{label} must be non-empty")
    if len(stripped) > max_length:
        raise ValueError(f"{label} must be at most {max_length} characters")
    return stripped


def squad_id(value: str) -> str:
    """Validate and return a non-empty squad identifier.

    The returned value is stripped and has a maximum length of 256 characters.
    """
    return _trimmed(value, max_length=256, label="squad_id")


# Bech32 alphabet used by Nostr npub1... public keys.
_BECH32_CHARS = re.compile(r"^[qpzry9x8gf2tvdw0s3jn54khce6mua7l]+$")


def pubkey(value: str) -> str:
    """Validate and return a Nostr public key.

    Accepts either:

    - A bech32 ``npub1...`` string (prefix and character set are validated).
    - A 64-character lowercase hexadecimal string.

    The input is returned unchanged.
    """
    if not isinstance(value, str):
        raise ValueError("pubkey must be a string")
    value = value.strip()

    if value.startswith("npub1"):
        data = value[5:]
        if not data:
            raise ValueError("pubkey npub data must be non-empty")
        if not _BECH32_CHARS.match(data):
            raise ValueError("pubkey contains invalid bech32 characters")
        return value

    if len(value) != 64:
        raise ValueError("pubkey must be npub1... or 64 hex characters")
    try:
        int(value, 16)
    except ValueError as exc:
        raise ValueError("pubkey must be 64 lowercase hex characters") from exc
    if value.lower() != value:
        raise ValueError("pubkey hex must be lowercase")
    return value


def event_id(value: str) -> str:
    """Validate and return an event identifier.

    Event identifiers must be non-empty and at most 128 characters.
    """
    return _trimmed(value, max_length=128, label="event_id")

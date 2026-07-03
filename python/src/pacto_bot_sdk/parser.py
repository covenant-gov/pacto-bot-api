"""Command parser for the pacto-bot-sdk Python SDK."""

from __future__ import annotations

from typing import Any


# Defensive limits for command parsing.
MAX_TOKENS = 256
MAX_TOKEN_BYTES = 1024
MAX_ARGS_FLAGS = 50


def parse_command(content: str) -> dict[str, Any] | None:
    """Parse a small command grammar out of message content.

    Returns ``None`` for empty/whitespace content or when the first token does
    not look like a command. The returned dict has keys ``command`` (str),
    ``args`` (list of str), and ``flags`` (dict of str to str or bool).

    Command syntax::

        /command arg1 arg2 --flag value --bool

    The leading ``/`` is stripped from the command. Tokens starting with
    ``--`` are flags; if the next token does not start with ``--`` it becomes
    the flag value, otherwise the flag is treated as boolean ``True``.
    """
    if not content or not content.strip():
        return None

    tokens = content.strip().split()
    if len(tokens) > MAX_TOKENS:
        tokens = tokens[:MAX_TOKENS]

    first = tokens[0]
    if not first.startswith("/"):
        return None

    command = first.lstrip("/")
    if not command:
        return None

    args: list[str] = []
    flags: dict[str, str | bool] = {}

    i = 1
    while i < len(tokens):
        token = tokens[i]
        if len(token.encode("utf-8")) > MAX_TOKEN_BYTES:
            i += 1
            continue

        if token.startswith("--"):
            key = token[2:]
            if i + 1 < len(tokens) and not tokens[i + 1].startswith("--"):
                flags[key] = tokens[i + 1]
                i += 2
            else:
                flags[key] = True
                i += 1
        else:
            if len(args) < MAX_ARGS_FLAGS:
                args.append(token)
            i += 1

        if len(args) >= MAX_ARGS_FLAGS and len(flags) >= MAX_ARGS_FLAGS:
            break

    return {"command": command, "args": args, "flags": flags}


__all__ = ["parse_command", "MAX_TOKENS", "MAX_TOKEN_BYTES", "MAX_ARGS_FLAGS"]

"""Small internal logger for the pacto-bot-sdk Python SDK."""

from __future__ import annotations

import os
import sys

_LEVELS = {
    "DEBUG": 10,
    "INFO": 20,
    "WARN": 30,
    "WARNING": 30,
    "ERROR": 40,
}


class Logger:
    """Tiny level-aware logger that writes to ``sys.stderr``.

    Levels are ``DEBUG``, ``INFO``, ``WARN``, and ``ERROR``. The effective level
    is resolved from the constructor argument, the ``PACTO_LOG_LEVEL``
    environment variable, or the default ``INFO``.
    """

    def __init__(self, bot_id: str, log_level: str | None = None) -> None:
        self.bot_id = bot_id
        self.level = self._resolve_level(log_level)

    def _resolve_level(self, level: str | None) -> int:
        if level is None:
            level = os.environ.get("PACTO_LOG_LEVEL", "info")
        return _LEVELS.get(level.upper(), _LEVELS["INFO"])

    def set_level(self, level: str | None) -> None:
        """Change the effective level at runtime."""
        self.level = self._resolve_level(level)

    def is_enabled(self, level: str) -> bool:
        """Return whether ``level`` would be emitted."""
        return self.level <= _LEVELS.get(level.upper(), _LEVELS["INFO"])

    def log(self, level: str, message: str) -> None:
        """Write ``message`` at ``level`` if enabled."""
        if not self.is_enabled(level):
            return
        print(
            f"[{self.bot_id}] {level.upper()}: {message}",
            file=sys.stderr,
            flush=True,
        )

    def debug(self, message: str) -> None:
        self.log("debug", message)

    def info(self, message: str) -> None:
        self.log("info", message)

    def warn(self, message: str) -> None:
        self.log("warn", message)

    def error(self, message: str) -> None:
        self.log("error", message)

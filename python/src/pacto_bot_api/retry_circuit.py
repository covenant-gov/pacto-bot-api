"""Retry-with-circuit-breaker state machine for the Pacto Python SDK."""

from __future__ import annotations

import random
import time
from dataclasses import dataclass, field
from enum import Enum, auto
from typing import Callable


class _State(Enum):
    CLOSED = auto()
    OPEN = auto()
    HALF_OPEN = auto()


@dataclass
class RetryCircuit:
    """Pure-state retry/circuit-breaker helper.

    Tracks consecutive failures and backs off with exponential delay plus
    jitter. After ``failure_threshold`` consecutive failures the circuit opens
    for ``cooling_off_seconds``. Once that period elapses the circuit becomes
    half-open and allows a single probe attempt. A successful attempt closes
    the circuit; a failed attempt reopens it.
    """

    retry_initial_backoff: float = 1.0
    retry_max_backoff: float = 30.0
    retry_jitter_ratio: float = 0.2
    circuit_failure_threshold: int = 5
    circuit_cooling_off_seconds: float = 60.0
    degraded_log_interval: float = 60.0

    _state: _State = field(default=_State.CLOSED, init=False, repr=False)
    _failure_count: int = field(default=0, init=False, repr=False)
    _opened_at: float | None = field(default=None, init=False, repr=False)
    _time_fn: Callable[[], float] = field(default=time.monotonic, init=False, repr=False)

    def __post_init__(self) -> None:
        if self.circuit_failure_threshold <= 0:
            raise ValueError("circuit_failure_threshold must be positive")
        if self.retry_initial_backoff < 0:
            raise ValueError("retry_initial_backoff must be non-negative")
        if self.retry_max_backoff < 0:
            raise ValueError("retry_max_backoff must be non-negative")
        if self.retry_jitter_ratio < 0:
            raise ValueError("retry_jitter_ratio must be non-negative")
        if self.circuit_cooling_off_seconds < 0:
            raise ValueError("circuit_cooling_off_seconds must be non-negative")
        if self.degraded_log_interval < 0:
            raise ValueError("degraded_log_interval must be non-negative")

    @classmethod
    def configure(
        cls,
        retry_initial_backoff: float = 1.0,
        retry_max_backoff: float = 30.0,
        retry_jitter_ratio: float = 0.2,
        circuit_failure_threshold: int = 5,
        circuit_cooling_off_seconds: float = 60.0,
        degraded_log_interval: float = 60.0,
    ) -> "RetryCircuit":
        """Construct a circuit from the standard settings object."""
        return cls(
            retry_initial_backoff=retry_initial_backoff,
            retry_max_backoff=retry_max_backoff,
            retry_jitter_ratio=retry_jitter_ratio,
            circuit_failure_threshold=circuit_failure_threshold,
            circuit_cooling_off_seconds=circuit_cooling_off_seconds,
            degraded_log_interval=degraded_log_interval,
        )

    def record_success(self) -> None:
        """Close the circuit and reset failure state."""
        self._state = _State.CLOSED
        self._failure_count = 0
        self._opened_at = None

    def record_failure(self) -> None:
        """Increment failures and open the circuit when threshold is reached."""
        self._failure_count += 1
        if self._failure_count >= self.circuit_failure_threshold:
            self._open()

    def next_action(self) -> tuple[bool, float]:
        """Return whether the caller should attempt a connection and how long to wait.

        The returned ``wait_seconds`` is:
        - the remaining cooling-off period while the circuit is open,
        - zero when the circuit is half-open,
        - zero when the circuit is closed and no failures have occurred yet,
        - a jittered exponential backoff when the circuit is closed and there
          have been prior failures.
        """
        if self._state == _State.OPEN:
            elapsed = self._time_fn() - self._opened_at
            if elapsed >= self.circuit_cooling_off_seconds:
                self._state = _State.HALF_OPEN
                return True, 0.0
            return False, self.circuit_cooling_off_seconds - elapsed

        if self._state == _State.HALF_OPEN:
            return True, 0.0

        if self._failure_count == 0:
            return True, 0.0

        base = self.retry_initial_backoff * (2 ** (self._failure_count - 1))
        base = min(base, self.retry_max_backoff)
        return True, self._jittered_backoff(base)

    @property
    def is_open(self) -> bool:
        return self._state == _State.OPEN

    @property
    def is_half_open(self) -> bool:
        return self._state == _State.HALF_OPEN

    @property
    def is_closed(self) -> bool:
        return self._state == _State.CLOSED

    @property
    def failure_count(self) -> int:
        return self._failure_count

    @property
    def opened_at(self) -> float | None:
        return self._opened_at

    def _open(self) -> None:
        self._state = _State.OPEN
        self._opened_at = self._time_fn()

    def _jittered_backoff(self, base: float) -> float:
        if base <= 0:
            return 0.0
        jitter = base * self.retry_jitter_ratio * (2.0 * random.random() - 1.0)
        return max(0.0, base + jitter)


__all__ = ["RetryCircuit"]

"""Tests for the RetryCircuit retry/circuit-breaker helper."""

from __future__ import annotations

import pytest

from pacto_bot_api.retry_circuit import RetryCircuit


@pytest.fixture
def circuit():
    return RetryCircuit(
        retry_initial_backoff=1.0,
        retry_max_backoff=8.0,
        retry_jitter_ratio=0.2,
        circuit_failure_threshold=3,
        circuit_cooling_off_seconds=60.0,
        degraded_log_interval=60.0,
    )


def test_invalid_failure_threshold_rejected():
    with pytest.raises(ValueError, match="circuit_failure_threshold must be positive"):
        RetryCircuit(circuit_failure_threshold=0)


def test_negative_config_values_rejected():
    with pytest.raises(ValueError, match="retry_initial_backoff must be non-negative"):
        RetryCircuit(retry_initial_backoff=-1)
    with pytest.raises(ValueError, match="retry_max_backoff must be non-negative"):
        RetryCircuit(retry_max_backoff=-1)
    with pytest.raises(ValueError, match="retry_jitter_ratio must be non-negative"):
        RetryCircuit(retry_jitter_ratio=-0.1)
    with pytest.raises(ValueError, match="circuit_cooling_off_seconds must be non-negative"):
        RetryCircuit(circuit_cooling_off_seconds=-1)
    with pytest.raises(ValueError, match="degraded_log_interval must be non-negative"):
        RetryCircuit(degraded_log_interval=-1)


def test_success_resets_failure_count_and_backoff(circuit):
    circuit.record_failure()
    circuit.record_failure()
    circuit.record_success()
    assert circuit.failure_count == 0
    assert circuit.is_open is False
    assert circuit.is_half_open is False

    should, wait = circuit.next_action()
    assert should is True
    # After a reset there are no failures, so the next attempt should be
    # immediate (wait = 0) rather than waiting for the initial backoff.
    assert wait == 0.0


def test_backoff_doubles_up_to_cap():
    circuit = RetryCircuit(
        retry_initial_backoff=1.0,
        retry_max_backoff=8.0,
        retry_jitter_ratio=0.2,
        circuit_failure_threshold=10,  # keep circuit closed throughout the test
        circuit_cooling_off_seconds=60.0,
        degraded_log_interval=60.0,
    )
    waits = []
    for _ in range(5):
        should, wait = circuit.next_action()
        assert should is True
        waits.append(wait)
        circuit.record_failure()

    # Backoff after each failure is 0 (initial), 1, 2, 4, 8 (capped). With
    # jitter ratio 0.2 each wait is within 20% of the base at that point.
    assert waits[0] == 0.0
    assert 1.0 * 0.8 <= waits[1] <= 1.0 * 1.2
    assert 2.0 * 0.8 <= waits[2] <= 2.0 * 1.2
    assert 4.0 * 0.8 <= waits[3] <= 4.0 * 1.2
    assert 8.0 * 0.8 <= waits[4] <= 8.0 * 1.2


def test_jitter_bounded_by_ratio(circuit):
    for _ in range(50):
        circuit.record_failure()
        should, wait = circuit.next_action()
        assert should is True
        base = 1.0
        assert abs(wait - base) <= circuit.retry_jitter_ratio * base + 1e-9
        circuit.record_success()


def test_circuit_opens_at_threshold(circuit):
    for _ in range(circuit.circuit_failure_threshold - 1):
        circuit.record_failure()
        assert circuit.is_open is False

    circuit.record_failure()
    assert circuit.is_open is True
    assert circuit.opened_at is not None


def test_open_state_blocks_attempts_until_cooling_off_elapses(circuit):
    now = 0.0
    circuit._time_fn = lambda: now

    for _ in range(circuit.circuit_failure_threshold):
        circuit.record_failure()

    assert circuit.is_open is True
    should, wait = circuit.next_action()
    assert should is False
    assert wait == pytest.approx(60.0)

    now = 30.0
    should, wait = circuit.next_action()
    assert should is False
    assert wait == pytest.approx(30.0)

    now = 60.0
    should, wait = circuit.next_action()
    assert should is True
    assert wait == pytest.approx(0.0)
    assert circuit.is_half_open is True


def test_zero_cooling_off_immediately_half_open(circuit):
    circuit.circuit_cooling_off_seconds = 0.0
    for _ in range(circuit.circuit_failure_threshold):
        circuit.record_failure()
    assert circuit.is_open is True
    should, wait = circuit.next_action()
    assert should is True
    assert wait == 0.0
    assert circuit.is_half_open is True


def test_successful_probe_closes_circuit(circuit):
    now = 0.0
    circuit._time_fn = lambda: now

    for _ in range(circuit.circuit_failure_threshold):
        circuit.record_failure()
    now = 60.0
    circuit.next_action()
    assert circuit.is_half_open is True

    circuit.record_success()
    assert circuit.is_open is False
    assert circuit.is_half_open is False
    assert circuit.failure_count == 0


def test_failed_probe_reopens_circuit_and_restarts_cooling_off(circuit):
    now = 0.0
    circuit._time_fn = lambda: now

    for _ in range(circuit.circuit_failure_threshold):
        circuit.record_failure()
    now = 60.0
    circuit.next_action()
    assert circuit.is_half_open is True

    now = 75.0
    circuit.record_failure()
    assert circuit.is_open is True
    should, wait = circuit.next_action()
    assert should is False
    assert wait == pytest.approx(60.0)
    assert circuit.opened_at == pytest.approx(75.0)


def test_success_while_closed_keeps_circuit_closed(circuit):
    circuit.record_failure()
    circuit.record_success()
    circuit.record_success()
    assert circuit.is_open is False
    assert circuit.failure_count == 0


def test_zero_degraded_log_interval_allowed():
    circuit = RetryCircuit(degraded_log_interval=0.0)
    assert circuit.degraded_log_interval == 0.0


def test_zero_failure_threshold_rejected():
    with pytest.raises(ValueError, match="circuit_failure_threshold must be positive"):
        RetryCircuit(circuit_failure_threshold=0)


def test_configure_class_method_returns_instance():
    circuit = RetryCircuit.configure(retry_initial_backoff=2.0)
    assert isinstance(circuit, RetryCircuit)
    assert circuit.retry_initial_backoff == 2.0


def test_integration_state_sequence_matches_bot_loop():
    """Simulate a Bot loop that fails, waits, probes, and recovers."""
    now = 0.0
    circuit = RetryCircuit(
        retry_initial_backoff=1.0,
        retry_max_backoff=8.0,
        retry_jitter_ratio=0.0,  # disable jitter for deterministic test
        circuit_failure_threshold=2,
        circuit_cooling_off_seconds=10.0,
    )
    circuit._time_fn = lambda: now

    # First attempt fails: closed, wait 0s before first retry.
    should, wait = circuit.next_action()
    assert should is True and wait == 0.0
    circuit.record_failure()

    # Second attempt fails: closed, wait 1s before next retry.
    should, wait = circuit.next_action()
    assert should is True and wait == 1.0
    circuit.record_failure()

    # Threshold reached: open, wait 10s.
    should, wait = circuit.next_action()
    assert should is False and wait == 10.0
    assert circuit.is_open is True

    # Cooling off elapsed: half-open, no wait.
    now = 15.0
    should, wait = circuit.next_action()
    assert should is True and wait == 0.0
    assert circuit.is_half_open is True

    # Probe succeeds: closed, reset.
    circuit.record_success()
    assert circuit.is_open is False
    assert circuit.failure_count == 0

    # Next failure resets back to initial backoff.
    should, wait = circuit.next_action()
    assert should is True and wait == 0.0
    circuit.record_failure()
    should, wait = circuit.next_action()
    assert should is True and wait == 1.0

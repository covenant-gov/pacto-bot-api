---
date: 2026-07-02
topic: sdk-reconnection-resilience
---

## Summary

Strengthen the Python SDK's connection handling so a bot handler survives daemon restarts, network blips, and transient errors without relying solely on Docker/systemd restart policies. Wrap the registration and read loops in retry logic with exponential backoff, jitter, and a circuit breaker that trips after a configurable number of consecutive failures and enters a cooling-off period before probing again. When the circuit is open, the bot logs a clear degraded state and continues to honor shutdown signals.

## Problem Frame

The SDK today (`python/src/pacto_bot_api/bot.py`) reconnects after a disconnect using a fixed exponential backoff capped at 8 seconds, but it has no jitter and no failure ceiling. A daemon that is restarting or a network path that is flapping can produce a tight sequence of reconnection attempts that:

- Hammer the daemon or Unix socket during startup.
- Fill logs with repetitive connection-failure noise.
- Leave operators with no visible signal about whether the bot is actively retrying or has given up.

Meanwhile, the scaffold templates (`templates/python/docker-compose.yml`, `templates/python/systemd.service`) rely on `restart: on-failure` / `Restart=on-failure` as the primary recovery mechanism. That works, but it is slower and coarser than in-process resilience: every restart re-runs imports, re-parses CLI args, and re-initializes the handler. Keeping retry logic in the bot process reduces restart storms and gives the operator a clear degraded state.

## Key Decisions

- **Retry wraps both registration and read loops.** The initial registration attempt and the long-running read loop after a successful registration both enter the same retry/circuit-breaker path, so a daemon that is briefly down during bot startup is handled the same way as a mid-run disconnect.
- **Exponential backoff with capped jitter.** Backoff doubles up to a maximum and adds randomized jitter to prevent thundering-herd reconnects when many bots restart at once.
- **Circuit breaker after N consecutive failures.** A configurable threshold trips the breaker into an open state; subsequent attempts are skipped for a cooling-off period. This caps log noise and socket pressure.
- **Half-open probe after cooling-off.** After the cooling-off period, the next scheduled attempt becomes a probe: if it succeeds, the breaker closes; if it fails, the breaker reopens and the cooling-off period repeats.
- **Degraded state is visible to operators.** When the circuit is open, the bot logs a single, clear degraded-state message and periodically emits a short status line. No metrics endpoint or HTTP surface is required in the first slice.
- **Shutdown signals always win.** SIGINT/SIGTERM immediately abort the retry sleep and close the circuit breaker path, so the bot does not hang during a cooling-off window.

## Requirements

### Retry and backoff

- R1. The SDK retry logic applies to both the initial registration call and the read-loop reconnect path in `python/src/pacto_bot_api/bot.py`.
- R2. Retry backoff is exponential with a configurable initial value and cap, plus randomized jitter bounded by a configurable ratio of the current backoff.
- R3. Backoff and jitter are reset to defaults after any successful connection and registration.
- R4. The retry loop sleeps on `asyncio.Event` / `asyncio.wait_for` so a shutdown signal can interrupt the sleep immediately.

### Circuit breaker

- R5. A configurable `failure_threshold` counts consecutive failed connection or registration attempts. When the threshold is reached, the circuit breaker opens.
- R6. While the circuit is open, the bot skips reconnection attempts and logs a clear degraded-state message at a reduced cadence.
- R7. After a configurable `cooling_off_seconds` period, the circuit enters a half-open state and makes one probe attempt.
- R8. If the probe succeeds, the circuit closes and normal operation resumes; if it fails, the circuit reopens and the cooling-off period repeats.
- R9. A successful connection or registration at any time resets the failure counter and closes the circuit.

### Observability and state

- R10. When the circuit opens, the bot logs a single message naming the daemon transport, the failure count, and the cooling-off duration.
- R11. While the circuit remains open, the bot logs a short status line no more than once per minute (configurable) so operators can see it is alive but degraded.
- R12. When the circuit closes, the bot logs a recovery message.
- R13. The degraded state is exposed via a simple property/method on `Bot` so handler code can optionally inspect it (e.g., `bot.is_degraded`).

### Configuration

- R14. The bot accepts retry/circuit-breaker settings via `Bot` constructor keyword arguments and standard CLI flags (`--retry-max-backoff`, `--retry-jitter`, `--circuit-failure-threshold`, `--circuit-cooling-off-seconds`, `--degraded-log-interval`).
- R15. Defaults are chosen for developer convenience: max backoff ~30s, jitter 0.1–0.3, failure threshold ~5, cooling-off ~60s, degraded log interval ~60s.
- R16. The scaffold templates (`templates/python/docker-compose.yml`, `templates/python/systemd.service`) continue to include `restart: on-failure` / `Restart=on-failure` as a safety net, but the primary recovery path is now in-process.

### Shutdown behavior

- R17. SIGINT and SIGTERM set the shutdown event, which breaks any active retry sleep and bypasses the circuit breaker so the bot exits cleanly.
- R18. During shutdown, the bot closes the transport and client even if the circuit breaker is open.

## Key Flows

- F1. Daemon restarts during normal operation
  - **Trigger:** The daemon process restarts and the bot's read loop receives an EOF or connection error.
  - **Actors:** Bot handler, SDK retry/circuit logic, daemon.
  - **Steps:** The SDK catches the error, logs a disconnect, waits with jittered backoff, and reconnects. If the daemon comes back within a few attempts, registration succeeds and the bot resumes dispatch.
  - **Outcome:** The bot survives the restart without a container/systemd restart cycle.

- F2. Repeated failures trip the circuit breaker
  - **Trigger:** The daemon or socket remains unavailable across several consecutive reconnection attempts.
  - **Actors:** Bot handler, SDK circuit breaker.
  - **Steps:** Each failure increments the counter. Once the threshold is reached, the circuit opens, the bot logs a degraded-state message, and it stops attempting to connect for the cooling-off period.
  - **Outcome:** Log noise and socket pressure are capped; the operator has a clear signal that the bot is degraded.

- F3. Recovery after cooling-off
  - **Trigger:** The cooling-off period elapses.
  - **Actors:** Bot handler, SDK circuit breaker.
  - **Steps:** The circuit enters half-open and makes one probe connection/registration attempt. If it succeeds, the circuit closes and the read loop runs normally. If it fails, the circuit reopens and cooling-off restarts.
  - **Outcome:** The bot automatically recovers when the daemon is reachable again.

- F4. Operator shuts down a degraded bot
  - **Trigger:** The operator sends SIGINT or SIGTERM while the circuit is open.
  - **Actors:** Bot handler, SDK shutdown signal handler.
  - **Steps:** The shutdown event fires, breaking any retry sleep and skipping the cooling-off wait. The transport and client are closed and the process exits.
  - **Outcome:** Shutdown remains responsive even during a long cooling-off period.

## Scope Boundaries

### Deferred for later

- Metrics endpoint or Prometheus-style metrics for reconnection counts and circuit state.
- Per-attempt hooks or pluggable retry strategies for custom bot logic.
- Advanced jitter strategies beyond simple ratio-based jitter.
- Backpressure or rate-limit coordination across multiple bots sharing a host.

### Outside this product's identity

- This feature is about SDK resilience, not about changing the daemon's transport protocol, the admin CLI, or the JSON-RPC contract.
- It does not replace Docker/systemd restart policies; it complements them.

## Acceptance Examples

- AE1. Covers R1, R2, R4.
  - **Given:** A bot is running and the daemon is temporarily unreachable.
  - **When:** The bot attempts to reconnect.
  - **Then:** The delay between attempts increases exponentially and each delay is jittered by up to 30% of the current backoff value; a shutdown signal cancels the wait immediately.

- AE2. Covers R5, R6, R7, R8.
  - **Given:** A bot is configured with `circuit_failure_threshold=3` and `circuit_cooling_off_seconds=5` and the daemon is down.
  - **When:** Three consecutive connection attempts fail.
  - **Then:** The bot logs a degraded-state message, stops attempting to connect for 5 seconds, then makes one probe attempt; if the daemon is still down, it logs the degraded state again and waits another 5 seconds.

- AE3. Covers R9, R12.
  - **Given:** The circuit is open and the daemon is down.
  - **When:** The daemon comes back up during the cooling-off period and the probe attempt succeeds.
  - **Then:** The circuit closes, the bot logs a recovery message, and normal read-loop dispatch resumes.

- AE4. Covers R17, R18.
  - **Given:** The circuit is open and the bot is in the middle of a 60-second cooling-off period.
  - **When:** The operator sends SIGTERM.
  - **Then:** The bot exits immediately without waiting for the cooling-off period to finish and closes the transport cleanly.

- AE5. Covers R13, R16.
  - **Given:** A bot is running with `--circuit-failure-threshold 5` and the daemon is down.
  - **When:** Five consecutive connection attempts fail and handler code checks `bot.is_degraded`.
  - **Then:** `bot.is_degraded` returns `True` and the scaffold defaults still include `restart: on-failure` in `docker-compose.yml`.

## Dependencies / Assumptions

- The Python SDK (`python/src/pacto_bot_api/`) remains the target for this work; no other language SDKs are in scope.
- The existing `asyncio` event-loop and signal-handler infrastructure in `bot.py` can be extended without a major refactor.
- The transport layer (`python/src/pacto_bot_api/transports.py`) exposes connection failures as `OSError`, `TimeoutError`, or empty readline (`""`) consistently across Unix and HTTP transports.
- The daemon's Unix socket and HTTP endpoints behave the same after a restart as after a fresh start; no daemon-side protocol changes are required.

## Outstanding Questions

- **Deferred to planning:** Exact default values for max backoff, jitter ratio, failure threshold, cooling-off period, and degraded log interval.
- **Deferred to planning:** Whether the circuit breaker state should be exposed through the `Bot` public API as a property, a method, or an event callback.
- **Deferred to planning:** Whether to extract the retry/circuit logic into a standalone helper class or keep it inline in `_run` for simplicity.
- **Resolved in this doc:** Both initial registration and read-loop reconnects enter the same retry/circuit path; Docker/systemd restart remains a safety net.

## Sources / Research

- `python/src/pacto_bot_api/bot.py` — existing reconnect loop in `_run` and signal-handler wiring.
- `python/src/pacto_bot_api/transports.py` — `UnixTransport.readline` returning `""` on EOF and `HttpTransport` disconnect behavior.
- `templates/python/docker-compose.yml` — `restart: on-failure` policy used as a safety net.
- `templates/python/systemd.service` — `Restart=on-failure` policy used as a safety net.
- `docs/brainstorms/2026-06-30-bot-scaffold-requirements.md` — adjacent scaffold work that defines how generated bots are configured and run.

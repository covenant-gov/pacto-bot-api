---
date: 2026-06-29
topic: python-sdk-seed
---

# Python SDK Seed + 30-Line Bot Template

## Summary

Create a single-file Python SDK helper at `examples/pacto_sdk.py` that hides JSON-RPC framing, transport setup, registration, and response lifecycle behind a callback-based client. Authors register command handlers with `client.on('/hello', handle_hello)` and return plain response dicts. The SDK supports both Unix socket and HTTP+SSE transports, includes a small command parser, and provides helpers for `agent.send_dm` and `agent.set_profile`. A `examples/greeting_bot.py` demonstrates a complete bot in roughly 30 lines of business logic.

## Problem Frame

`examples/echo_bot.py` is the only copy-pasteable Python reference for the daemon. It is ~290 lines of standard-library code, and roughly the first 150 lines re-implement JSON-RPC framing, connection management, `handler.register`, the outbound write loop, shutdown signals, and response dispatch. Every new bot author currently copies that plumbing before writing any bot logic. This raises the barrier for non-Rust developers and makes every subsequent example harder to review. A small, standard-library-only helper would make the API feel like a real SDK and let examples focus on behavior rather than protocol wiring.

## Key Decisions

- **Callback + command registry instead of decorators.** The SDK exposes `client.on('/hello', handle_hello)` rather than `@bot.command('/hello')`. This avoids decorator magic while still offering command parsing, and it leaves room for a future decorator layer without breaking the callback API.
- **Both Unix socket and HTTP+SSE transports from the start.** The SDK abstracts both transports behind the same callback API. This validates transport parity and removes deployment friction for remote handlers, at the cost of parsing SSE streams and managing the `X-Pacto-Handler-Id` header over HTTP.
- **Plain dicts instead of typed dataclasses.** Events, responses, and notification parameters travel as plain dicts, with SDK helper functions that return those dicts. This keeps carrying cost low and makes manual schema drift less painful until a generated Python client arrives.
- **Single file in `examples/` rather than a package.** The SDK lives at `examples/pacto_sdk.py` so it can be imported by every example without packaging overhead. It is explicitly a seed, not a published package, and can be promoted to a package layout once it stabilizes.
- **`greeting_bot.py` supplements `echo_bot.py`; it does not replace it.** `echo_bot.py` remains the stdlib-only reference implementation and the anchor for the existing contract-test harness.

## Requirements

### SDK surface and API

- R1. `examples/pacto_sdk.py` provides a callback-based client class that authors instantiate and import into their bot files.
- R2. The client exposes a command registry: `client.on('/hello', handler)` registers an async callback for messages whose content starts with `/hello`.
- R3. The SDK parses a command into a command name, positional arguments, and flags; the exact supported syntax is documented.
- R4. The SDK handles JSON-RPC framing, `handler.register`, connection lifecycle, shutdown signals, and the outbound write loop automatically.
- R5. The SDK provides helper functions or methods that return plain dicts for `handler.response` actions (`ack`, `reply`, `defer`, `ignore`) and for `agent.send_dm` and `agent.set_profile` notifications.

### Transport support

- R6. The SDK supports Unix socket transport by default, deriving the socket path from `$PACTO_SOCKET`, `$PACTO_DATA_DIR`, or a constructor argument.
- R7. The SDK supports HTTP+SSE transport via a constructor flag or environment variable, including `X-Pacto-Handler-Id` header management for mutating methods and SSE event parsing for inbound `agent.event` notifications.

### Example bot and tests

- R8. `examples/greeting_bot.py` demonstrates a complete bot in approximately 30 lines of business logic, responding to `/hello` with a friendly message.
- R9. `examples/greeting_bot.py` includes a matching `examples/greeting_bot.manifest.json` and passes the existing contract-test harness.
- R10. `examples/echo_bot.py` remains unchanged as the stdlib-only reference implementation and continues to pass its existing contract test.

### Documentation and compatibility

- R11. `examples/pacto_sdk.py` uses only the Python standard library, matching `examples/echo_bot.py`.
- R12. A short README section or inline docstring explains that `pacto_sdk.py` is a manual seed, not the eventual generated client, and points to the generated-client plan.

## Key Flows

- F1. Bot startup
  - **Trigger:** author runs `python greeting_bot.py`.
  - **Steps:** parse environment/constructor arguments, connect the selected transport, send `handler.register`, start the read loop.
  - **Outcome:** the bot is ready to receive `agent.event` notifications.
  - **Covered by:** R1, R4, R6, R7, R8.

- F2. Command dispatch
  - **Trigger:** the daemon sends `agent.event` whose `content` starts with a registered command prefix.
  - **Steps:** parse content into command name, positional args, and flags; find the registered callback; await the callback; send the returned `handler.response` dict over the transport.
  - **Outcome:** the daemon receives a terminal response for the event.
  - **Covered by:** R2, R3, R5.

- F3. Proactive notification
  - **Trigger:** a handler decides to call `agent.send_dm` or `agent.set_profile`.
  - **Steps:** build the notification dict via an SDK helper; enqueue and send it over the transport.
  - **Outcome:** the daemon receives and authorizes the mutating call.
  - **Covered by:** R5.

## Acceptance Examples

- AE1. Greeting command over Unix socket (covers R8, R9)
  - **Given:** a daemon with bot id `greeting-bot` configured and running over a Unix socket.
  - **When:** the contract harness injects an `agent.event` with `content: "/hello"`.
  - **Then:** `greeting_bot.py` sends a `handler.response` notification with `action: "reply"` and content containing a friendly greeting.

- AE2. Greeting command over HTTP+SSE (covers R7, R8, R9)
  - **Given:** a daemon configured for HTTP transport and a valid `$PACTO_SECRET_TOKEN`.
  - **When:** `greeting_bot.py` starts with the HTTP transport selected and the harness injects an `agent.event` for `/hello`.
  - **Then:** the bot registers via HTTP POST, consumes `/events` via SSE, and replies with a `handler.response` that includes the correct `X-Pacto-Handler-Id` header on mutating calls.

- AE3. `echo_bot.py` remains intact (covers R10)
  - **Given:** the current `examples/echo_bot.py` and `examples/echo_bot.manifest.json`.
  - **When:** `cargo test` or `pytest examples/test_examples_contract.py` runs.
  - **Then:** the echo bot contract test passes with no modifications to `echo_bot.py` or its manifest.

## Scope Boundaries

### Deferred for later

- Generated Python client derived from `schemas/jsonrpc.json`.
- Typed dataclasses or Pydantic models for events and responses.
- Decorator-based routing sugar on top of the callback registry.
- Additional example bots such as poll, multi-persona receptionist, or profile mood ring.
- HTTP transport contract tests for examples beyond `greeting_bot.py`.

### Outside this product's identity

- Publishing the SDK as a standalone PyPI package.
- Replacing `examples/echo_bot.py` as the stdlib-only reference.

## Dependencies / Assumptions

- `schemas/jsonrpc.json` is the source of truth for JSON-RPC method and notification shapes.
- The daemon's HTTP transport already supports SSE delivery and requires the `X-Pacto-Handler-Id` header for mutating methods (`src/transport/http.rs`).
- The existing contract-test harness in `examples/test_examples_contract.py` discovers `examples/**/*_bot.py` and executes matching manifest files (`docs/plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md`).
- A generated Python client is intentionally deferred; `pacto_sdk.py` is a temporary manual seed.

## Outstanding Questions

- None. The exact command-parser syntax is deferred to planning.

## Sources / Research

- `examples/echo_bot.py` — current stdlib-only reference showing JSON-RPC framing, registration, write loop, and shutdown plumbing.
- `schemas/jsonrpc.json` — OpenRPC 1.3.1 catalog of daemon methods, notifications, params, and result schemas.
- `schemas/example-manifest.json` — schema for per-bot example manifests used by the contract harness.
- `docs/plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md` — plan that defers a generated Python client and defines the manifest/contract harness.
- `docs/ideation/2026-06-29-example-bots-ideation.html` — ideation artifact proposing `examples/pacto_sdk.py` + `examples/minimal_bot.py`.

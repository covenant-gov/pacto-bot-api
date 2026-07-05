---
date: 2026-06-28
topic: python-examples-ci-contract-tests
---

# Python Examples CI Contract Tests

> **Historical note:** The root `examples/` directory was consolidated into `python/` after this document was written. The legacy standard-library seed SDK (`examples/pacto_sdk.py`) and the associated example bots were removed; the generated Python SDK is now published to PyPI as `pacto-bot-sdk`. The contract-test harness lives in `python/tests/`. See `CHANGELOG.md` [Unreleased] for the consolidation details.

## Summary

Add a CI job that discovers every example bot in `examples/`, validates each against a per-bot manifest of JSON-RPC contract pieces over the Unix socket, uses the PR's daemon binary artifact produced by the Rust build step, and reports results as an allowed-to-fail informational check that graduates to a required gate once stable.

## Problem Frame

Bot developers copy `examples/` as reference implementations. Today those examples are not exercised in CI, so they can drift from the daemon's actual JSON-RPC contract without anyone noticing. The first signal of drift is usually a confused user rather than a failing build. The project already has pytest fixtures that spawn the daemon and speak the contract, so the missing piece is automated, repeatable CI execution with a stable definition of what each example must demonstrate.

## Key Decisions

- **Per-example manifest over strict template.** Examples can demonstrate partial or specialized behaviors without all looking like `echo_bot`. A manifest declares which contract pieces each bot exercises.
- **One manifest file per bot.** Each discovered bot has its own manifest at `examples/<bot_name>.manifest.json`. This keeps manifests colocated with the bots they describe and avoids a single global file that every example must edit.
- **Unix socket transport only.** This matches the existing `examples/conftest.py` fixtures and the primary developer path. HTTP contract tests for examples are deferred.
- **Allowed-to-fail initially with graduation criteria.** The job runs on every PR but does not block merge. It is promoted to a required gate after 30 consecutive days with zero manifest-related failures.
- **Use a daemon binary artifact from the Rust build step.** The Python job receives the path to a built `pacto-bot-api` binary (for example, `target/debug/pacto-bot-api` or an uploaded CI artifact), matching the intent of local `pytest examples/` behavior without relying on Cargo-internal environment variables in a separate CI job.

## Requirements

### Contract harness

- R1. The CI job discovers every example bot file matching `examples/**/*_bot.py` recursively.
- R1a. The CI job fails with a clear diagnostic when a discovered bot has no corresponding manifest entry.
- R2. The job reads a manifest at `examples/<bot_name>.manifest.json` for each bot, declaring which contract pieces the example exercises and any expected errors.
- R3. The harness exercises registration, clean shutdown, and any declared contract pieces by default. A manifest may opt out of registration and/or shutdown for partial or specialized examples (for example, connection-error handling or startup-diagnostics bots). Contract-piece examples are illustrative; the actual set is declared per manifest.
- R4. The harness supports declaring expected-error outcomes for examples that intentionally demonstrate unauthorized or error paths.
- R4a. Expected errors match by JSON-RPC error code, with an optional message substring for disambiguation.
- R5. Each contract piece has a deterministic timeout and produces a pass/fail result with a clear diagnostic on failure. The default timeout is 30 seconds; individual pieces may override it in the manifest.
- R5a. The harness spawns a fresh daemon process for each example bot and shuts it down cleanly after the example completes, preventing cross-example state leakage.

### CI integration

- R6. The job runs on every pull request and on every push to `main`.
- R7. The job uses a `pacto-bot-api` binary artifact produced by the Rust build step (for example, `target/debug/pacto-bot-api` or a CI-uploaded artifact).
- R8. The job runs over Unix socket transport only.
- R9. The job is allowed-to-fail initially and reports its results as a separate, named CI check via a custom status-check submission step, so a failing run is visible without blocking merge.
- R9a. The job is promoted to a required gate after 30 consecutive days with zero manifest-related failures on `main`.
- R10. CI installs Python 3.10+ and the dependency set declared in `examples/requirements.txt`.
- R17. The Rust CI job builds and uploads a `pacto-bot-api` binary artifact on every pull request so the examples job does not recompile the daemon.
- R18. The examples harness generates test keys locally or uses a pre-built `pacto-bot-admin` binary; it does not invoke `cargo run` per discovered example.
- R19. The harness creates each Unix socket in a temporary directory under `/tmp` with a path well under the AF_UNIX limit and removes the socket after each example.

### Manifest versioning

- R11. Each manifest declares its own `manifest_version`, independent of the daemon's JSON-RPC schema version. The harness rejects manifests whose declared version is not supported by `schemas/example-manifest.json`.
- R12. When `schemas/example-manifest.json` changes, the examples CI job validates every manifest against the updated schema and fails on any unsupported contract piece or field. Changes to `schemas/example-manifest.json` require a manifest-format review via required PR review.
- R13. The manifest schema is defined in `schemas/example-manifest.json` and versioned separately from `schemas/jsonrpc.json`.

## Acceptance Examples

- AE1. A new `examples/greet_bot.py` that registers, receives a matching `agent.event`, and emits a `handler.response` with expected action and payload passes CI without adding per-bot test code.
- AE2. An example that declares an expected JSON-RPC error code (for example, `-32006` for an unauthorized bot) for a missing capability passes when the daemon returns that code.
- AE3. A change to `schemas/jsonrpc.json` that removes a method or field used by an example's manifest causes the examples CI job to fail with a contract-assertion failure naming the affected example and missing declaration.

## Scope Boundaries

### Deferred for later

- HTTP transport contract tests for examples.
- Generated Python client derived from `schemas/jsonrpc.json`.
- Windows CI examples job.
- `status` contract-piece verb and any bot-status-specific examples.

### Outside this product's identity

- Rewriting the example bots in another language (they stay Python).
- Changing the daemon's transport layer to support this work.

## Dependencies / Assumptions

- `examples/conftest.py` can be extended to support parameterized example discovery and manifest-driven assertions.
- The Rust CI job produces a usable `pacto-bot-api` binary before the Python job runs.
- Unix sockets are available in GitHub Actions `ubuntu-latest` runners.
- The examples harness can spawn a fresh daemon process per example bot and clean it up reliably.

## Sources / Research

- `examples/conftest.py` — pytest fixtures that spawn the daemon and register a handler over the Unix socket.
- `examples/test_echo_bot.py` — existing contract test for the echo bot.
- `examples/echo_bot.py` — reference bot that consumes `handler.register` and `agent.event` notifications.
- `schemas/jsonrpc.json` — canonical JSON-RPC method catalog that the examples rely on.
- `.github/workflows/ci.yml` — current Rust-only CI pipeline.

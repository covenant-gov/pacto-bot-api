---
date: 2026-06-29
topic: generated-python-client
---

# Generated Python Client for pacto-bot-api

## Summary

Create a generated Python SDK under `python/` that is produced from `schemas/jsonrpc.json` via `cargo xtask codegen`. The SDK exposes Pydantic models for every JSON-RPC type and a low-level async client, plus a hand-written high-level bot layer with decorators and command parsing. Example bots live in `python/examples/` and are excluded from the PyPI wheel. Documentation is written for both humans and LLMs, so an LLM pointed at the repo can infer how to register a handler, dispatch commands, and send replies.

## Problem Frame

`examples/pacto_sdk.py` is a hand-written, stdlib-only seed. It was useful for bootstrapping examples, but every new JSON-RPC method or schema change forces a manual update, and the file is already positioned as temporary in `docs/plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md`. Meanwhile the Rust side of the daemon already generates types from `schemas/jsonrpc.json` through `cargo xtask codegen` and enforces sync via `tests/schema_sync.rs`. There is no equivalent typed, discoverable Python surface.

The longer-term goal is to make the Python SDK the primary way authors write Pacto bots. Documentation should be good enough that an LLM can read the package README and examples and generate working bot code without reading the daemon source.

## Key Decisions

- **Python generator invoked by `cargo xtask codegen`.** A Python script reads `schemas/jsonrpc.json` and emits the package. This keeps the developer-facing command unified while letting Python maintainers own the output shape, docstrings, and Pydantic models.
- **Generated low-level layer + hand-written high-level layer.** The generator produces Pydantic models and a low-level async client. A hand-written layer on top provides decorators, a command registry, and bot-class ergonomics.
- **In-repo package under `python/`.** Keeping the SDK in the daemon repo prevents schema drift and lets the contract harness consume it before any PyPI publication.
- **Examples in `python/examples/`.** New example bots live with the SDK but are excluded from the wheel. Future examples can be showcased on a website without bloating the package.
- **`examples/pacto_sdk.py` is no longer maintained.** It may stay in place while the author migrates personal usage, but new features and methods will not be added to it.
- **AI-first documentation.** README, module docstrings, and examples are written so an LLM can infer the full bot-authoring workflow from reading them.

## Requirements

### Code generation

- R1. `cargo xtask codegen` invokes a Python generator that reads `schemas/jsonrpc.json` and emits the client package under `python/`.
- R2. The generator emits Pydantic v2 models for every JSON-RPC method parameter, result, notification, and event type declared in `schemas/jsonrpc.json`.
- R3. The generator emits a low-level async client class with one typed method per JSON-RPC method (for example, `handler_register`, `agent_send_dm`, `agent_set_profile`).
- R4. Generated method and model names use Pythonic `snake_case` and include stable docstrings derived from the schema summary and description fields.

### SDK package structure

- R5. The package is installable in editable mode from the repo root via `pyproject.toml` (for example, `pip install -e python/`).
- R6. The hand-written high-level layer lives in the same package and provides a decorator-based bot class (for example, `@bot.command("/hello")`) and a small command parser.
- R7. The high-level layer supports both Unix socket and HTTP+SSE transports using the generated low-level client.
- R8. The package exposes a clear public API: `from pacto_bot_api import Bot, PactoClient` or equivalent.

### Examples

- R9. Example bots live in `python/examples/` and import the generated package.
- R10. At least one example demonstrates a complete bot in roughly 30 lines of business logic using the high-level layer.
- R11. Example files are excluded from the PyPI wheel via `pyproject.toml` package configuration.

### Documentation / AI-first design

- R12. The package README includes installation instructions, a quickstart snippet, and links to every example.
- R13. Every generated model and client method includes a docstring with its JSON-RPC method name, a one-line summary, and a minimal usage example.
- R14. The README and docstrings present the canonical bot-authoring loop: register → receive `dm_received` event → dispatch command → return `handler.response`.
- R15. Examples include docstrings and inline comments that explain what the bot does, what capabilities it needs, and how to run it.

### Schema sync and CI

- R16. `tests/schema_sync.rs` fails if the generated Python client is stale relative to `schemas/jsonrpc.json`.
- R17. The Python generator can be run idempotently: running it twice on the same schema produces no diff.

## Key Flows

- F1. Regenerate after schema change
  - **Trigger:** A developer adds or changes a JSON-RPC method in `schemas/jsonrpc.json`.
  - **Steps:** Run `cargo xtask codegen`; the Python generator rewrites the low-level client and models; `tests/schema_sync.rs` passes only if the committed output matches.
  - **Outcome:** The Python SDK stays atomically in sync with the daemon contract.

- F2. Author a new bot with the high-level layer
  - **Trigger:** An author wants a new bot.
  - **Steps:** Create a file in `python/examples/`; import `Bot` from `pacto_bot_api`; register commands with decorators; run the file.
  - **Outcome:** The bot connects, registers, dispatches commands, and replies without the author touching JSON-RPC framing.

- F3. LLM reads the SDK and generates a bot
  - **Trigger:** An LLM is pointed at the SDK repo or documentation site.
  - **Steps:** The LLM reads the README quickstart, a complete example, and the decorator docstrings; it emits a new example-sized bot file.
  - **Outcome:** The emitted bot runs against the daemon with only environment-specific edits (bot id, socket path, secret).

## Acceptance Examples

- AE1. Schema sync catches drift
  - **Given:** A committed generated client and a developer who edits `schemas/jsonrpc.json` without regenerating.
  - **When:** `cargo test --test schema_sync` runs in CI.
  - **Then:** The test fails with a clear message indicating the Python client is stale.
  - **Covers:** R16.

- AE2. New example from README
  - **Given:** A reader who has installed the package in editable mode and started the daemon.
  - **When:** They copy the README quickstart into `python/examples/my_bot.py` and run it.
  - **Then:** The bot registers and replies to its documented command.
  - **Covers:** R12, R14.

- AE3. LLM can infer a complete bot
  - **Given:** The package README and one decorated-command example.
  - **When:** An LLM is prompted to write a bot that responds to `/joke` with a reply.
  - **Then:** The generated code imports the public API, registers `/joke`, returns a reply action, and requires no daemon-specific imports.
  - **Covers:** R13, R14, R15.

## Scope Boundaries

### Deferred for later

- Publishing the SDK to PyPI and semantic-versioning it independently of the daemon.
- Generating clients for languages other than Python.
- A public website that showcases examples.
- Removing `examples/echo_bot.py` and `examples/pacto_sdk.py`; they stay until the author finishes migrating personal usage.

### Outside this product's identity

- A standalone SDK repository divorced from the daemon schema.
- A language-agnostic binding layer such as a gRPC/Protobuf wrapper or C ABI.

## Dependencies / Assumptions

- `schemas/jsonrpc.json` remains the canonical source of truth for the JSON-RPC contract.
- Python 3.10 or newer is the supported floor for the SDK.
- Pydantic v2 is acceptable as a runtime dependency.
- The daemon's existing `cargo xtask codegen` command is the entry point developers already use.

## Outstanding Questions

- None.

## Sources / Research

- `schemas/jsonrpc.json` — canonical OpenRPC 1.3.1 catalog of daemon methods and types.
- `xtask/src/codegen.rs` — existing Rust code generator that emits `src/transport/protocol_generated.rs` from `schemas/jsonrpc.json`.
- `tests/schema_sync.rs` — CI-enforced gate that verifies generated Rust types match the schema.
- `examples/pacto_sdk.py` — hand-written SDK seed that the generated client supersedes.
- `docs/plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md` — plan that explicitly defers a generated Python client.
- `docs/plans/2026-06-29-002-feat-python-sdk-seed-plan.md` — plan for the hand-written seed, which also notes the generated client is deferred.

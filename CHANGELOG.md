# Changelog

All notable changes to `pacto-bot-api` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-06-30

### Added

- `pacto-bot-admin scaffold` subcommand and `pacto-bot-admin new --scaffold`
  flag for generating opinionated Python bot handler projects from templates
  under `templates/python/`.
- Multi-stage Dockerfile packaging both `pacto-bot-api` and `pacto-bot-admin`
  binaries, running as a non-root `pacto` user with a `/var/lib/pacto-bot-api`
  volume.
- GHCR image publish jobs in CI on pushes to `main` and release tags.
- `.dockerignore` to keep Docker build context small.

### Changed

- Interactive `pacto-bot-admin new` wizard now asks whether to scaffold a
  handler project and where to place it; when scaffolding, the generated
  `pacto-bot-api.toml` is written into the project directory.
- `python-pacto-bot` skill now directs agents to start new Python bot projects
  with `pacto-bot-admin new --scaffold` instead of hand-writing files.
- Bumped `rusqlite` from 0.34.0 to 0.40.1.
- Bumped `jsonschema` from 0.30.0 to 0.46.6.

### Fixed

- CI Docker image job no longer runs on pull requests; images are built and
  pushed only on `main` branch pushes and release tags.

## [0.2.0] - 2026-06-29

### Added

- Interactive `pacto-bot-admin new` wizard that prompts for backend, relays,
  capabilities, and optional profile fields when no `bot_id` is supplied.
- Bot profile fields `display_name`, `about`, and `picture` in
  `pacto-bot-api.toml`; `pacto-bot-admin publish-profile` uses them when
  building kind:0 metadata.
- LLM-readable operator guide via `pacto-bot-admin --llm-help` and the generated
  `docs/pacto-bot-admin-llms.txt`.
- Per-command `after_help` examples and operator notes for every
  `pacto-bot-admin` command.
- `cargo xtask docs` to regenerate `docs/pacto-bot-admin-llms.txt`.
- Single-file Python SDK seed at `examples/pacto_sdk.py`: stdlib-only Unix
  socket and HTTP+SSE transports, command parser/registry, and response helpers.
- Generated Python SDK under `python/`, produced from `schemas/jsonrpc.json`
  via `cargo xtask codegen`. It exposes typed Pydantic models, a low-level async
  `PactoClient`, and a high-level decorator-based `Bot` API.
- Reference Python bots:
  - `examples/greeting_bot.py` using the seed SDK.
  - `python/examples/greeting_bot.py` and `python/examples/joke_bot.py` using
    the generated SDK.
- `python-pacto-bot` skill for SDK-aware bot authoring in Claude Code, Cursor,
  and Oh My Pi.
- Manifest-driven example contract-test harness
  (`schemas/example-manifest.json`, `examples/test_examples_contract.py`) that
  discovers and validates `examples/**/*_bot.py` and `python/examples/*_bot.py`.
- CI jobs for Python SDK tests and example contract tests.
- `CODEOWNERS` review gate for `schemas/example-manifest.json`.
- Dependabot cargo configuration.

### Changed

- `pacto-bot-admin new` now takes an optional `bot_id`; omitting it starts the
  interactive wizard instead of erroring.
- `pacto-bot-admin publish-profile` uses `display_name` (falling back to the
  bot id) and optional `about`/`picture` fields for kind:0 content.
- README and `DEVELOPMENT.md` rewritten to feature the generated Python SDK,
  reference examples, and bot-authoring workflow.

### Fixed

- Release install script defaults to `logicminds/pacto-bot-api` and correctly
  verifies checksums with the `dist/` prefix.
- Config file permission enforcement handles relative config paths correctly.

### Security

- Added admin CLI creation tests that verify `nsec` values are not leaked in
  stdout/stderr when creating bunker-backed bot identities.

## [0.1.0] - 2026-06-28

### Added

- Initial `pacto-bot-api` daemon: a standalone Rust/Tokio service that multiplexes multiple Pacto bot identities over a shared Nostr backend.
- JSON-RPC 2.0 handler API over newline-delimited frames on two transports:
  - Unix domain socket at `$DATA_DIR/pacto-bot-api.sock` with `0o600` permissions.
  - Optional localhost HTTP server at `127.0.0.1:9800` protected by `X-Pacto-Bot-Secret`.
- `ClientManager` for static multi-bot configuration loaded from `pacto-bot-api.toml`.
- Three signing backends per bot identity:
  - Local test key (`nsec`) for development, with `zeroize` clearing on drop.
  - Local NIP-46 bunker.
  - Remote production NIP-46 bunker with strict `npub` mismatch rejection.
- `HandlerRegistry` and fan-out event dispatch: handlers register for event types and bot identities, and events are dispatched to all matching handlers.
- Per-call capability authorization for mutating operations (`agent.send_dm`, `agent.set_profile`, `agent.error`).
- Per-handler (10 ops/sec, burst 20) and per-bot aggregate rate limiting.
- SQLite persistence in WAL mode for cursors, handler registrations, and config, with restart recovery and `npub` mismatch detection.
- `pacto-bot-admin` CLI for bot lifecycle operations: `new`, `publish-profile`, `test-bunker`, `export`, `import`, `rotate-http-token`, `status`, and `diagnose --format json`.
- Machine-readable contract artifacts under `schemas/` (config, JSON-RPC catalog, metrics, service compatibility).
- Structured runtime metrics via `agent.metrics` and periodic `$DATA_DIR/reports/latest.json` flushes.
- Graceful shutdown on SIGTERM/SIGINT with cursor persistence and `agent.status` notifications.
- Default test suite running in-process against mock relay and mock bunker implementations, plus gated integration tests for `pacto-dev-env`.
- Secret-redaction test suite verifying that `nsec`, bunker URIs, and the HTTP token never leak into logs, error responses, or binary strings.

### Security

- Established Phase 1 trust boundaries: Unix socket uses kernel file permissions; HTTP transport uses a CSPRNG-generated 256-bit hex secret stored with `0o600` permissions.
- Config file permissions enforced (`0o600` or stricter) on daemon startup.
- Daemon-wide exclusive lock on `$DATA_DIR/daemon.lock` to prevent concurrent instances.

[Unreleased]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/covenant-gov/pacto-bot-api/releases/tag/v0.1.0

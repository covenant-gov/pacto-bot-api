# Changelog

All notable changes to `pacto-bot-api` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Daemon now fans out `mls_welcome_received` events to handlers that subscribe to them, enabling bots to react when they join a Squad.
- Python SDK `Bot` gains an opt-in squad join greeting via `hello_message="..."`. When the bot is invited to a Squad and has the `SendGroupMessages` capability, it automatically sends the configured message to the group. Use `{bot_id}` in the message to include the bot's identity.
- Python SDK `@bot.on_squad_join` decorator for custom handling of `mls_welcome_received` events; this overrides the built-in `hello_message` auto-response and adds the event type to the handler subscription.

### Changed

- `schemas/jsonrpc.json` now lists `mls_welcome_received` as a valid `agent.event` type and clarifies that `chat_id` contains the Squad wire id for both welcome and group messages.

## [0.8.0] - 2026-07-17

### Added

- New `ExitMlsGroup` capability and `agent.exit_mls_group` JSON-RPC method let a bot publish a self-removal MLS proposal to leave a Squad. Requires an MLS engine (`mls_db_path`) and the `ExitMlsGroup` capability.
- Python SDK gains `bot.exit_squad(group_id)` high-level helper, which calls `agent.exit_mls_group` and returns the published evolution event id.
- The daemon now accepts MLS `Welcome` messages on its own behalf and skips handler fan-out for those messages, so a bot that joins a Squad does not receive the daemon's own welcome.

### Changed

- Bumped `tower-http` from `0.6.11` to `0.7.0`.
- Bumped `toml` from `0.8.23` to `1.1.3` (spec 1.1.0 compliant).

### Fixed

- `send_group_message` now resolves the wire group id to the MLS group id before dispatching, fixing group-message delivery for newly created Squads.
- Python SDK `Bot` reconnects now fall back to a fresh `handler.register` when the daemon rejects a stale reconnect token, preventing a bot from getting stuck after the daemon is recreated.

## [0.7.0] - 2026-07-09

### Added

- Python SDK usability improvements (Bosun Phase 2 review):
  - `@bot.event(type)` and `@bot.dm` decorators route `agent.event` notifications by `event.type` without subclassing `Bot`.
  - `@bot.hears(token)` decorator matches plain-text commands by the first token exactly.
  - Guaranteed acknowledgement: decorated handlers returning `None` automatically send `handler_response(action="ignore")` when `auto_acknowledge=True` (the new default). `bot.ignore(event)` and `bot.reply(event, content)` helpers return the canonical response dict.
  - `bot.own_pubkey` property populated from the daemon's `handler.register`/`handler.reconnect` response, which now includes an `own_pubkeys` map.
  - High-level helpers `bot.send_group_message(group_id, content)` and `bot.is_squad_member(group_id, member_pubkey)`.
  - Optional per-request timeouts on the generated `PactoClient` with a 30-second default.
  - `@bot.throttle(key, window_seconds)` and `@bot.lock(name, on_conflict=..., max_waiters=...)` decorators for in-memory per-key throttling and serialized handler execution.
  - New `pacto_bot_sdk.validate` module with `squad_id`, `pubkey`, and `event_id` validators.
  - Unknown daemon notification types are now logged at warning level once per type.
  - README updated with the handler-response contract and decorator examples.
- Daemon `handler.register` and `handler.reconnect` responses now include `own_pubkeys: dict[str, str]` mapping each registered `bot_id` to its npub.

### Changed

- `Bot(...)` now defaults to `auto_acknowledge=True`. Existing bots that rely on the previous no-response semantics for `None` returns can set `auto_acknowledge=False` during a transition.
- Improved MLS KeyPackage error reporting in `pacto-bot-admin` and JSON-RPC responses:
  - A missing recipient KeyPackage now returns a dedicated `KeyPackageNotFound` error (`-32017`) instead of the generic `Nostr relay error` (`-32004`), with a message naming the recipient and pointing to kind:443 freshness requirements.
  - `InvalidKeyPackage` (signature, kind, or author mismatch) is now mapped to `-32018` so it is distinct from the missing-package case.

### Removed

- Removed the Rust `crates/governance-bot` example crate from the workspace. The reference governance snapshot bot implementation now lives in `pacto-governance-bots` (Python).

### Security

- Server-assigned handler IDs: `handler.register` no longer accepts client-supplied `handler_id`, preventing takeover of existing registrations.
- Reconnect token enforcement: `handler.reconnect` requires a secret server-generated token, and live takeovers of already-connected handlers are rejected.
- Authorized handler unregister: `agent.unregister_handler` now requires the caller to have the `Admin` capability or be the target handler itself.

### Fixed

- Move blocking SQLite and filesystem I/O off the async runtime: config load, diagnostics report flush, daemon lock acquisition, HTTP secret token creation, Unix socket setup, and SQLite cursor/handler operations now run on Tokio's blocking thread pool or via async filesystem APIs.
- Replace the `std::sync::Mutex<Database>` in `Dispatch` with an async `Db` wrapper that runs blocking SQLite work on worker threads, preventing async runtime threads from being blocked by the SQLite mutex.
- Exit daemon gracefully when the Nostr event stream ends (`None`) instead of looping indefinitely while unresponsive.
- `NostrClient::shutdown` now stops the underlying relay-pool notification loop so `receive_events` streams terminate cleanly.
- `MockRelay` closes active WebSocket connections on `stop()`, improving integration-test coverage of relay-loss scenarios.

## [0.6.0] - 2026-07-04

### Changed

- Consolidated Python examples and the contract-test harness under `python/`.
  - Moved `examples/conftest.py`, `examples/test_examples_contract.py`, and `examples/test_manifest_validation.py` to `python/tests/`.
  - The parameterized contract harness now discovers only `python/examples/*_bot.py`.
  - Removed the legacy standard-library seed SDK (`examples/pacto_sdk.py`), `examples/echo_bot.py`, and the root `examples/` directory.
  - CI installs Python dev dependencies via `pip install -e python/[dev]` and runs contract tests from `python/`.

## [0.5.0] - 2026-07-03

### Added

- Python SDK reconnection resilience for `Bot`:
  - New `RetryCircuit` helper with exponential backoff, jitter, failure ceiling, and circuit-breaker states (closed/open/half-open).
  - Retry/circuit settings configurable via `Bot` constructor kwargs and CLI flags (`--retry-initial-backoff`, `--retry-max-backoff`, `--retry-jitter-ratio`, `--circuit-failure-threshold`, `--circuit-cooling-off-seconds`, `--degraded-log-interval`).
  - Initial registration and runtime reconnects share the same retry/circuit path.
  - Degraded state exposed via `Bot.is_degraded` and logged with a single open message, periodic status lines, and a recovery message.
  - Shutdown signals interrupt retry sleeps and bypass the circuit breaker for clean exit.
- End-to-end handler lifecycle management: visibility, self-healing registrations, stale-handler reaping, and operational cleanup.
  - `pacto-bot-admin handlers list` shows every registered handler with `handler_id`, `bot_ids`, `event_types`, `capabilities`, `transport`, `state`, `connected`, `last_seen`, and `registered_at`.
  - `pacto-bot-admin handlers show <handler_id>` returns a single handler's details.
  - `pacto-bot-admin handlers unregister <handler_id>` forcibly removes a stale handler from the routing table.
  - `pacto-bot-admin status` now includes the same `handlers` array in JSON output and prints handler details in text output.
  - New JSON-RPC admin methods `agent.list_handlers` and `agent.unregister_handler` expose the daemon's routing table to the CLI.
  - Handler records track `transport`, `last_seen`, `registered_at`, and a `connected`/`disconnected` state; the reaper removes disconnected handlers after a configurable timeout (default 30s).
  - The Python SDK `Bot` loop now automatically reconnects and re-registers after a daemon restart, reusing the same `handler_id` across reconnects.
  - The Python SDK's `PactoClient` read loop exits on transport disconnect so the reconnect loop can run.

### Changed

- `python-pacto-bot` skill moved from the main repository to the `pacto-bot-templates` repository (`python-llm/project/.agents/skills/python-pacto-bot/`). The scaffold generator now copies the skill into generated projects from the selected template. Old copies under `skills/python-pacto-bot/`, `.agents/skills/python-pacto-bot/`, `.claude/skills/python-pacto-bot/`, and `.omp/skills/python-pacto-bot/` have been removed.
- Python scaffold `docker-compose.yml` now always runs the daemon + bot stack by default.
- Generated scaffold config uses environment-variable placeholders (`${PACTO_RELAY_URL:-...}`, `${PACTO_BUNKER_URI:-...}`, `${PACTO_DATA_DIR:-...}`, `${PACTO_SOCKET_PATH:-...}`) so the same `pacto-bot-api.toml` works inside Docker Compose and on the host.
- Daemon config parser now supports `${ENV_VAR:-default}` syntax in addition to `${ENV_VAR}`.
- Scaffolded bot containers connect to the daemon via a shared Docker volume (`pacto-socket`) instead of mounting the host socket, eliminating UID/GID permission mismatches.
- Generated README and AGENTS docs updated to describe the new default stack, internal/external relay options, and the `PACTO_RELAY_URL` environment variable.
- Python SDK Unix-transport error messages now reference the default Docker Compose workflow instead of the removed `bot-only` profile.

### Fixed

- Makefile `validate` target comment now correctly describes that it runs `fmt-check` and `clippy` (tests are run via `make test`).

## [0.4.1] - 2026-07-01

### Changed

- Python scaffold `docker-compose.yml` now pulls the Nostr relay and NIP-46 bunker images from `ghcr.io/covenant-gov/pacto-dev-env` instead of requiring local builds.
- Generated project documentation updated to explain that relay and bunker images are pulled from GHCR and no longer built from `pacto-dev-env` locally.

### Fixed

- GHCR Docker images are now built and published for both `linux/amd64` and `linux/arm64`, so `docker pull` works on Apple Silicon and other ARM64 hosts.

## [0.4.0] - 2026-06-30

### Added

- `pacto-bot-admin new --scaffold` now generates self-contained Python handler projects:
  - Project-level `README.md`, `AGENTS.md`, and per-bot `AGENTS.md` for agent-friendly onboarding.
  - Vendored Python SDK source under `sdk/` plus a built wheel available inside containers.
  - Copy of the `python-pacto-bot` skill under `skills/python-pacto-bot/`.
  - `.gitignore` and `.dockerignore` templates to keep `pacto-bot-api.toml` and other secrets out of git and Docker contexts.
- `--project-name` flag for `pacto-bot-admin new --scaffold` as a convenience alias for `--project-dir`.
- `--http` flag for scaffolded bots that call external HTTP APIs.
- `manifest.json` contract harness for scaffolded projects.
- `parse_command` export, `reply_on_error` helper, and optional HTTP dependencies in the Python SDK.
- Generated scaffold config uses environment-variable placeholders (`${PACTO_RELAY_URL:-...}`, `${PACTO_BUNKER_URI:-...}`, `${PACTO_DATA_DIR:-...}`, `${PACTO_SOCKET_PATH:-...}`) so the same `pacto-bot-api.toml` works inside Docker Compose and on the host.
- Scaffolded bot containers connect to the daemon via a shared Docker volume (`pacto-socket`) instead of mounting the host socket, eliminating UID/GID permission mismatches.
- Generated README and AGENTS docs updated to describe the new default stack, internal/external relay options, and the `PACTO_RELAY_URL` environment variable.
- Python SDK Unix-transport error messages now reference the default Docker Compose workflow instead of the removed `bot-only` profile.

### Changed

- Default scaffold project directory changed from `<bot-id>` to `<bot-id>-project`, so generated bots live at `<project-dir>/bots/<bot-id>/` instead of the confusing `<bot-id>/bots/<bot-id>/`.
- Generated bot template now includes a `_command_args(event)` helper and subcommand dispatch guidance.
- Generated `docker-compose.yml` uses a single bot service with `bot-only` and `full` profiles instead of separate per-bot services.
- Dockerfile and compose build from the project root so the local SDK wheel is available inside containers.
- `python-pacto-bot` skill synced with SDK and scaffold updates.

### Fixed

- Scaffold template extraction no longer double-nests `include_dir` root-relative paths under the language directory.
- Template-tree recursion now uses the correct subtree and target directory.
- Removed redundant `force-include` from `python/pyproject.toml` that broke SDK wheel builds.

## [0.3.0] - 2026-06-30

### Added

- `pacto-bot-admin scaffold` subcommand and `pacto-bot-admin new --scaffold`
  flag for generating opinionated Python bot handler projects from external
  cargo-generate templates resolved from `pacto-bot-templates`.
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

- Release install script defaults to `covenant-gov/pacto-bot-api` and correctly
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

[Unreleased]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/covenant-gov/pacto-bot-api/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/covenant-gov/pacto-bot-api/releases/tag/v0.1.0

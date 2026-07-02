# CONCEPTS.md — pacto-bot-api

## Core entities

### Bot identity

A static Nostr identity configured in `pacto-bot-api.toml`. Each identity has:
- an `id` (operator-chosen name, e.g. `echo-bot`);
- an `npub` (public key);
- a signing backend (`nsec`, local NIP-46 bunker, or remote NIP-46 bunker);
- relay URLs;
- optional profile fields (`display_name`, `about`, `picture`).

The daemon reads identities at startup; it never creates or deletes them.

### BotState

Runtime state for one bot identity. Owned by `ClientManager`. Holds the npub, relay subscriptions, signing connection, and per-bot rate-limit state.

### Handler

A client process that registers with the daemon to receive events and issue replies. A handler:
- connects via Unix socket or localhost HTTP;
- calls `handler.register` with a list of `bot_ids`, `event_types`, and `capabilities`;
- receives `agent.event` notifications;
- replies with `agent.send_dm`, `agent.set_profile`, `agent.error`, or `handler.response`.

Handlers are authorized per call against their registered capabilities.

### HandlerRegistry

Daemon-side routing table. Maps active handler connections to their registered bots, event types, and capabilities. The reaper removes disconnected handlers after a configurable timeout.

### Dispatch

Fan-out logic that sends an incoming event to every handler whose registration matches the event type and bot identity. Waits for terminal handler responses or a dispatch timeout before advancing cursors.

## Transports

### Unix socket transport

Newline-delimited JSON-RPC 2.0 frames over `$DATA_DIR/pacto-bot-api.sock`. Created with `0o600` permissions. Best for co-located handlers and lowest latency.

### HTTP transport

Optional localhost-only server on `127.0.0.1:9800`. Requires the `X-Pacto-Bot-Secret` header. Useful for handlers that prefer HTTP or SSE-style streaming.

## Cryptography and signing

### NIP-46 bunker

A remote or local signing service that holds the bot's private key. The daemon connects to the bunker and asks it to sign events; the private key never enters daemon memory. Production bots must use this backend.

### `nsec` backend

Development-only backend that reads the raw private key from the `PACT_BOT_NSEC` environment variable. Key material is cleared from memory on drop using `zeroize`. Do not use in production.

### Gift wrap (kind:1059)

Encrypted Nostr event envelope used for sealed DMs. The daemon receives gift wraps, decrypts them, and forwards the inner event to matching handlers as a `dm_received` event.

## Capabilities

Authorization claims requested by a handler at registration. The daemon enforces them on every mutating call. Examples include:
- `ReadMessages` — receive incoming DMs;
- `SendMessages` — send outgoing DMs;
- `SetProfile` — publish kind:0 profile metadata.

## Persistence

### SQLite (`agent.db`)

WAL-mode SQLite database under `$DATA_DIR`. Stores:
- per-bot/event cursors;
- handler registrations;
- config snapshot;
- diagnostics and metrics history.

Cursor advancement waits for terminal handler responses so events are not lost across restarts.

### Reports

Periodic JSON dumps of runtime metrics and diagnostics to `$DATA_DIR/reports/latest.json`. Used by `pacto-bot-admin diagnose` and `status`.

## Admin CLI

### `pacto-bot-admin`

Lifecycle and operations CLI. Responsibilities are strictly separated from the daemon:
- `new` — create a bot identity;
- `scaffold` / `new --scaffold` — generate a Python handler project;
- `publish-profile` — publish kind:0 metadata;
- `test-bunker` — verify bunker connectivity and npub match;
- `export` / `import` — move daemon-local state between data dirs;
- `handlers` — list, show, or unregister handlers;
- `rotate-http-token` — rotate the HTTP secret;
- `diagnose` / `status` — operational health checks.

### `pacto-bot-api.toml`

Daemon configuration file. Must be `0o600` or stricter. Defines relays, bot identities, HTTP transport, rate limits, and handler-reaper timeouts. Supports `${ENV_VAR}` and `${ENV_VAR:-default}` interpolation.

## Code generation

### `schemas/`

Canonical JSON Schema and OpenRPC artifacts. Source of truth for:
- `config.json` — daemon config schema;
- `jsonrpc.json` — handler-facing JSON-RPC catalog;
- `example-manifest.json` — contract harness for examples.

### `src/*_generated.rs`

Rust types generated from `schemas/` by `cargo xtask codegen`. Do not hand-edit.

### `python/`

Generated Python SDK: Pydantic models, async `PactoClient`, and decorator-based `Bot` API. Regenerated from `schemas/jsonrpc.json`.

## Development patterns

### Schema-first evolution

Change the schema, run `cargo xtask codegen`, update callers/tests. `tests/schema_sync.rs` ensures generated files do not drift.

### Secret hygiene

Secrets (`nsec`, bunker URI, HTTP token) are represented with `secrecy::SecretString` or `zeroize::Zeroizing`, never logged, and never returned in error messages. A dedicated secret-redaction test suite verifies this.

### Docker-free default tests

The default `cargo test` suite runs in-process against mock relay and mock bunker implementations in `tests/support/`. Integration tests against real Docker services are gated with `#[ignore]` and require `PACTO_DEV_ENV=1`.

## Glossary

| Term | Meaning |
|------|---------|
| **npub** | Nostr public key (bech32-encoded). |
| **nsec** | Nostr private key (bech32-encoded). Dev-only backend. |
| **bunker URI** | NIP-46 connection string (e.g. `bunker://...`). |
| **cursor** | Persisted offset that tracks which events have been processed for a bot/event pair. |
| **fan-out** | Sending one event to all matching handlers. |
| **kind:0** | Nostr event kind for profile metadata. |
| **kind:1059** | Nostr event kind for gift-wrapped (sealed) DMs. |

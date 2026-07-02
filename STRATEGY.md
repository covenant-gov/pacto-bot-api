# STRATEGY.md — pacto-bot-api

## Target problem

Running a Pacto bot today requires orchestrating a lot of heavy, identical infrastructure: a Nostr relay pool, encrypted DM handling, NIP-46 signing, key material, and persistence. If an operator wants to run multiple bots, each one duplicates that backend. Bot developers also get tied to a specific language runtime instead of writing just the message-handling logic.

`pacto-bot-api` solves this by making the backend a single, standalone daemon that multiplexes many bot identities and exposes a small, language-agnostic JSON-RPC contract to handlers. The daemon owns the hard parts; handlers own the bot behavior.

## Approach

1. **One daemon, many bots.** A single Rust/Tokio process (`pacto-bot-api`) loads static bot identities from `pacto-bot-api.toml`, maintains a shared `nostr-sdk` relay pool, and manages one signing connection per bot.

2. **Language-agnostic handler API.** Handlers connect over a Unix domain socket or localhost HTTP and speak JSON-RPC 2.0. Incoming DMs are pushed as `agent.event` notifications; handlers reply with `agent.send_dm`, `agent.set_profile`, or `agent.error`. The contract is the source of truth in `schemas/`.

3. **Security-first defaults.** Config files are `0o600`; the Unix socket is `0o600`; the HTTP token is a CSPRNG-generated 256-bit hex secret; secrets are never logged or returned in errors; production signing uses NIP-46 bunkers, not local `nsec`.

4. **Admin CLI owns lifecycle.** `pacto-bot-admin` creates identities, publishes profiles, tests bunkers, rotates tokens, scaffolds handler projects, and imports/exports state. The daemon never creates or deletes identities.

5. **Schema-first evolution.** JSON Schema and OpenRPC artifacts in `schemas/` generate Rust types (`cargo xtask codegen`) and the Python SDK. CI enforces sync.

## Users

| User | What they do | How they interact |
|------|--------------|-------------------|
| **Bot operator** | Runs and secures the daemon | Configures `pacto-bot-api.toml`, manages keys/bunkers, runs `pacto-bot-admin` |
| **Bot developer** | Writes bot behavior | Connects a handler over JSON-RPC, or uses the generated Python SDK |
| **AI assistant** | Helps author or operate bots | Uses `pacto-bot-admin --llm-help` and the `python-pacto-bot` skill |

## Key metrics

- **Operational cost:** one relay pool and one signing connection per operator, not per bot.
- **Language friction:** bot logic can be written in any language that speaks JSON-RPC 2.0; first-class Python SDK is generated from the same schema as the Rust daemon.
- **Uptime:** handlers reconnect and re-register automatically after daemon restarts; stale handlers are reaped after a timeout.
- **Security surface:** secrets never leak in logs, errors, or binary strings; production keys stay in NIP-46 bunkers.

## Tracks of work

| Track | Status | Description |
|-------|--------|-------------|
| **Phase 1: daemon core** | Shipped | Multi-bot config, NIP-17/44/59 DMs, NIP-46 bunkers, handler registration, dispatch, SQLite persistence, admin CLI, Python SDK. |
| **Handler lifecycle management** | Shipped | `pacto-bot-admin handlers list/show/unregister`, daemon-side handler reaping, reconnect resilience in Python SDK. |
| **Reconnection resilience** | Shipped | `RetryCircuit` with backoff, jitter, and circuit breaker in Python SDK `Bot`. |
| **SDK/template decoupling** | In planning | Make the generated Python SDK and scaffold templates independently versionable and testable. See `docs/plans/2026-07-02-001-feat-sdk-reconnection-resilience-plan.md` and related brainstorms. |
| **Phase 2+** | Planned | MLS group participation, on-chain governance reads/writes, webhook delivery. |

## Version

Current crate version: `0.4.1` (see `Cargo.toml` and `CHANGELOG.md`).

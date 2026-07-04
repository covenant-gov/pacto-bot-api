# pacto-bot-api vs. NostrBotKit — comparison and borrowable ideas

Research date: 2026-07-01
Source project: https://codeberg.org/Tuxor/NostrBotKit (main branch, Rust crate `nbk` v0.5.2)

## Headline differences

| Dimension | pacto-bot-api (us) | NostrBotKit (them) |
|-----------|-------------------|-------------------|
| **Primary user** | Bot *developers* who write handler logic in any language | Bot *operators* who configure bots via DM and YAML |
| **Extension model** | JSON-RPC 2.0 handler programs (external processes) | Built-in command types in YAML (reply, post, api_call, dialog, menu, composite, rss_feed, etc.) |
| **Control surface** | `pacto-bot-admin` CLI + daemon JSON-RPC | DM commands + optional web admin UI |
| **Multi-bot model** | Static multi-bot config loaded from `pacto-bot-api.toml` | AlphaBot manager creates/starts/stops/deletes bots at runtime via DM |
| **Persistence** | SQLite (`agent.db`) with WAL, cursors, handler registrations | YAML files + flat `data/` directories |
| **Signing** | Local nsec (dev), local bunker, remote bunker; strict pubkey verification | Per-bot nsec in `.env`, referenced by `nsec_env` in YAML |
| **Group chat** | Planned Phase 2 (MLS) | Already ships Marmot (MLS/NIP-104) support |
| **Lightning / payments** | Not in Phase 1 | NWC (NIP-47), payment-gated commands, Zap notifications, lud16 |
| **Webhooks** | Not in Phase 1 (Phase 3) | Inbound webhooks + outbound `notify_url` callbacks today |
| **Cron / scheduling** | Not in Phase 1 | Built-in cron jobs, timezone-aware |
| **Media uploads** | Not in Phase 1 | NIP-96 + Blossom media upload UI and API |
| **Deployment** | Standalone binary + Docker + GHCR | Docker Compose with optional relay/Blossom sidecars |

## What NostrBotKit does that we might want to borrow

### 1. Built-in command catalog for non-programmers
NostrBotKit lets users create bots without writing code: `/addcmd trigger=/weather type=api_call url=... template=...`. This covers a large class of simple bots (reply, post, RSS, API calls, composite aggregations, menus, dialogs). We could add a similar *declarative command layer* on top of our JSON-RPC daemon, either as an optional "simple handler" built-in or as a scaffold template that generates a Python handler from YAML.

- **Use case:** An operator wants a bot that posts an RSS feed every hour without writing Python.
- **Borrowable form:** A `pacto-bot-admin add-command` CLI that writes a tiny YAML manifest, and a built-in or generated "declarative handler" that registers for the bot and implements the catalog.
- **Effort:** Medium if we reuse our scaffold generator; the core dispatch logic is already in place.

### 2. Inbound webhooks for external integrations
NostrBotKit's `POST /webhook/{bot_id}/{trigger}` lets external scripts trigger bot actions and return the result synchronously. This is in our Phase 3 plan but is a common early need.

- **Use case:** A CI job or monitoring system calls a webhook to make the bot post an alert or DM a recipient.
- **Borrowable form:** Add an optional localhost HTTP endpoint (or extend our existing HTTP transport) to accept authenticated webhooks and route them to a registered handler or directly to `agent.send_dm`/`agent.post`.
- **Effort:** Low-to-medium; our HTTP transport and auth already exist.

### 3. Outbound `notify_url` callbacks
Any NostrBotKit command can fire a JSON POST to an external URL after execution. This is a lightweight alternative to persistent event streaming.

- **Use case:** Forward all DMs to an analytics or logging endpoint, or chain a command result to another service.
- **Borrowable form:** Add an optional `notify_url` to handler registration or to `agent.send_dm`/`agent.event` configuration.
- **Effort:** Low.

### 4. Cron / scheduled jobs
NostrBotKit has timezone-aware cron jobs (`/addcron`) that execute commands on a schedule. This is a natural fit for many bot use cases.

- **Use case:** Daily digest, hourly status check, scheduled announcements.
- **Borrowable form:** A built-in scheduler that calls `agent.send_dm` or `agent.post` on behalf of a registered bot, or a generated handler that uses cron.
- **Effort:** Medium; requires persistence and a scheduler task.

### 5. Lightning / NWC integration
NostrBotKit supports payment-gated commands, lud16 Lightning addresses, and Zap notifications. This is likely important for our ecosystem but currently deferred.

- **Use case:** Premium bot commands, paid access, or donation notifications.
- **Borrowable form:** Optional NWC signer integration and payment events in the JSON-RPC catalog.
- **Effort:** Medium-to-high; touches signer, payment polling, and event handling.

### 6. Marmot / MLS group chat
NostrBotKit already supports MLS-encrypted groups via the Marmot protocol. Our plan explicitly defers MLS to Phase 2, but we can study their implementation (`src/router/handle_marmot.rs`, `mdk-core` usage).

- **Use case:** Bots participating in Pacto squads / MLS groups.
- **Borrowable form:** Reuse their relay/channel patterns and state storage approach when we implement Phase 2.
- **Effort:** High; already planned.

### 7. Blueprint / template system
NostrBotKit's blueprints let users share and re-apply bot configurations, with parametric setup dialogs. This is conceptually close to our scaffold generator but at runtime.

- **Use case:** Share a "weather bot" or "support bot" template, instantiate it with one command.
- **Borrowable form:** Extend our scaffold templates to support parameterized setup, and optionally add runtime bot creation from templates via the admin CLI.
- **Effort:** Medium.

### 8. Web admin UI
NostrBotKit has a browser-based admin UI at `/admin` for users who prefer a visual interface. We currently rely on CLI and JSON-RPC.

- **Use case:** Operators who are uncomfortable with the CLI want to manage bots, commands, and cron jobs.
- **Borrowable form:** A minimal static admin UI served on the HTTP transport, read-only or with limited actions.
- **Effort:** Medium; security considerations (no built-in password, must be behind a reverse proxy) mirror our HTTP transport warnings.

### 9. DM-first runtime control
NostrBotKit can create, start, stop, and delete bots at runtime via DM. Our daemon only loads static config and requires CLI edits + restart.

- **Use case:** Remote administration without SSH access.
- **Borrowable form:** This conflicts with our deliberate design (daemon never creates/deletes identities, KTD-8). We could add runtime config reload via SIGHUP or admin CLI, but should keep identity creation out of the daemon.
- **Effort:** Medium for hot-reload; identity creation is a non-goal.

### 10. State stored as plain files, not a database
NostrBotKit uses YAML + flat files. This makes backup/inspection trivial but sacrifices transactional cursor safety. Our SQLite choice is better for our scale and reliability goals, but we could borrow the idea of **human-readable export files** and easier version control of bot config.

- **Borrowable form:** Keep SQLite for runtime, but add a `pacto-bot-admin export-config` that emits a human-readable YAML/TOML snapshot.
- **Effort:** Low.

## What we do better and should not give up

- **Language-agnostic handlers:** Our JSON-RPC model lets developers write bots in any language. NostrBotKit is essentially configuration-only unless you fork the Rust project.
- **Security posture:** We enforce file permissions (`0o600` config), constant-time HTTP token comparison, `secrecy`/`zeroize`, bunker pubkey verification, and a dedicated secret-redaction test suite. NostrBotKit stores keys in `.env`, has no admin UI password, and logs webhook auth failures (per `src/webhook.rs`).
- **Schema-first contract:** Our `schemas/` are canonical, generate Rust and Python types, and are CI-gated. NostrBotKit documents commands but the wire format is internal to the Rust crate.
- **Multi-bot multiplexing from day one:** One daemon, one relay pool, one DB. NostrBotKit spins up a separate `nostr-sdk` `Client` per bot, which is heavier.
- **Capability model:** We enforce per-call authorization against registered capabilities. NostrBotKit's permission levels are simpler and rely on the bot's follow list.
- **Daemon vs. admin separation:** Our daemon never creates identities, reducing runtime attack surface. NostrBotKit's AlphaBot can create new bot identities via DM, which widens the trust boundary.
- **Structured diagnostics:** `agent.metrics`, `diagnose --format json`, and `reports/latest.json` are designed for agentic/automated operation.

## Concrete, actionable feature candidates (ranked)

1. **Inbound webhooks** (high value, low effort, Phase 3 precursor). Reuse existing HTTP transport and auth.
2. **Outbound `notify_url` / callback hooks** (high value, low effort). Add to `handler.register` or event config.
3. **Human-readable config export** (low effort, useful for backup/ops).
4. **Declarative command catalog / simple handler mode** (medium effort, broadens user base).
5. **Cron scheduler** (medium effort, common bot need).
6. **Lightning/NWC support** (medium-high effort, ecosystem fit).
7. **Web admin UI** (medium effort, operator convenience).
8. **Runtime bot templates / blueprints** (medium effort, reuse scaffold).

## Operational / architectural takeaways

- **NostrBotKit is an operator-centric appliance; pacto-bot-api is a developer-centric runtime.** The projects are not direct competitors; they optimize for different users. We can broaden our appeal by adding a non-programmer command layer without sacrificing the JSON-RPC core.
- **Their DM-first control is convenient but risky.** Running identity creation and config edits through the Nostr DM surface makes remote compromise more impactful. Our CLI/daemon separation is the safer default for production.
- **Their sidecar deployment model (relay + Blossom) is user-friendly.** We can improve our Docker Compose and dev-env setup to make "one command and you have a local relay" as easy as theirs.
- **Their state model (flat files) is simpler but less robust.** Our SQLite + WAL model is the right choice for reliability, but we should make exports and inspection as easy as theirs.
- **Marmot/MLS is a must-study.** Their implementation shows how to integrate `mdk-core` and `mdk-sqlite-storage` into a Tokio daemon; this directly informs our Phase 2 work.

## Sources

- NostrBotKit: https://codeberg.org/Tuxor/NostrBotKit (main branch, `nbk` v0.5.2)
- pacto-bot-api: local repository, `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`, `schemas/jsonrpc.json`, `Cargo.toml`, `AGENTS.md`

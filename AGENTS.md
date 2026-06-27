# Repository Guidelines

## Project Overview

`pacto-bot-api` is a planned standalone Rust daemon that multiplexes multiple Pacto bot identities onto one shared backend. Bot developers write handlers in any language and connect to the daemon over a language-agnostic JSON-RPC 2.0 API; the daemon owns Nostr relay connections, encrypted DM handling, signing keys, and message routing.

> **Current state:** This repository contains planning and architecture documentation only. No source code, build files, or tests exist yet. Treat the conventions below as the intended design extracted from `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` and the architecture deep dives.

## Architecture & Data Flow

```
pacto-bot-api daemon (Rust/Tokio)
├── ClientManager      # one BotState per configured bot identity
├── HandlerRegistry    # active handler_id → connection + capabilities
├── Event Dispatch     # fan-out by event type + bot npub
├── Transport Layer
│   ├── Unix socket    # $DATA_DIR/pacto-bot-api.sock, 0o600
│   └── localhost HTTP # 127.0.0.1:9800, X-Pacto-Bot-Secret
├── nostr-sdk Client   # shared relay pool + subscriptions
├── NIP-46 bunkers     # one signing connection per bot identity
└── SQLite (rusqlite)  # $DATA_DIR/agent.db — cursors, handlers, config
```

**Flow:**
1. The daemon reads static bot identities from `pacto-bot-api.toml`.
2. `ClientManager` creates one `BotState` per identity, connecting to relays and the configured signing backend.
3. Incoming `kind:1059` gift wraps are decrypted and forwarded as `agent.event` notifications to matching handlers.
4. Handlers reply via `agent.send_dm` / `agent.set_profile` / `agent.error`; the daemon verifies capabilities per-call, encrypts/wraps, and publishes.

Key pattern: **daemon manages runtime, admin CLI manages lifecycle**. The daemon never creates or deletes bot identities; `pacto-bot-admin` creates keys, publishes profiles, tests bunkers, and exports/imports state.

## Key Directories

| Path | Purpose |
|------|---------|
| `docs/` | Architecture research, implementation plans, and ecosystem setup guides. |
| `docs/plans/` | Formal feature plans and security reviews. |
| `src/` | Not present yet. Will contain the daemon, `ClientManager`, transports, and persistence. |
| `tests/` | Not present yet. Will contain in-process mock relay/bunker integration tests. |
| `schemas/` | Not present yet. Will contain canonical JSON Schema/OpenRPC contracts. |
| `xtask/` | Not present yet. Planned build/task runner (`cargo xtask codegen`). |

## Development Commands

Because the crate does not yet exist, these commands are the planned targets:

```bash
# Build and run the daemon
cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml --data-dir ./data

# Run the admin CLI
cargo run --bin pacto-bot-admin -- new my-bot
cargo run --bin pacto-bot-admin -- publish-profile my-bot
cargo run --bin pacto-bot-admin -- test-bunker my-bot
cargo run --bin pacto-bot-admin -- diagnose --format json

# Default test suite (in-process mocks, no Docker)
cargo test

# Gated integration tests against pacto-dev-env (Docker)
cargo test -- --ignored

# Codegen / full verification
cargo xtask codegen
```

Ecosystem-wide setup for local services (relay, EVM testnet, bunker):

```bash
cd dev-setup             # in a repo that provides it (pacto-app or pacto-dev-env)
docker compose up -d --build
docker compose --profile bunker up -d --build
```

## Code Conventions & Common Patterns

### Language & style
- Rust with Tokio async runtime.
- Use `snake_case` for JSON-RPC method/field names; Rust structs use `PascalCase` with `serde(rename_all = "snake_case")`.
- Two binary targets: `pacto-bot-api` (daemon) and `pacto-bot-admin` (CLI).

### Error handling
- Use standard Rust `Result` propagation; avoid panics for operational errors (config validation, relay failure, bunker mismatch).
- Errors returned to handlers are JSON-RPC 2.0 error objects; secrets must never appear in error messages.

### Secrets & cryptography
- Represent nsec, bunker URIs, and the HTTP secret token with `secrecy::SecretString` or `zeroize::Zeroizing`.
- `nsec` backend clears key material on drop with `zeroize`; still treated as dev-only.
- Never log secrets, config signing material, or the HTTP token.

### Async & state management
- `ClientManager` owns per-bot `BotState` (npub, relay subscriptions, bunker connection).
- `HandlerRegistry` owns active registrations and routing.
- SQLite in WAL mode persists cursors, handler registrations, and config; cursor advancement waits for terminal handler responses or dispatch timeout.

### Dependency injection
- Plan favors constructor injection for testability. Mock relay and mock bunker implementations live in `tests/support/` for the default test suite.

### Capability & authorization
- Handlers register for specific bot identities and capabilities.
- Every mutating call (`agent.send_dm`, `agent.set_profile`, `agent.error`) is authorized against the registration, not just at connection time.

## Important Files

| File | Purpose |
|------|---------|
| `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` | Primary implementation plan: requirements, architecture, JSON-RPC catalog, config schema, security invariants. |
| `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-executive-summary.md` | High-level concept and Phase 1 scope. |
| `docs/plans/2026-06-24-001-security-review-findings.md` | Security findings and resolutions. |
| `docs/GETTING_STARTED.md` | Ecosystem-wide local dev setup. |
| `docs/pacto-bot-architecture-deep-dive.md` | Background on Pacto backend and why the daemon is needed. |
| `docs/pacto-bot-architecture-deep-dive-2.md` | Predecessor design for `pacto-agent`/daemon. |
| `pacto-bot-api.toml` (planned) | Bot identity and capability config; must be `0o600` or stricter. |
| `$DATA_DIR/agent.db` (planned) | SQLite persistence. |
| `$DATA_DIR/daemon.lock` (planned) | Exclusive lock preventing concurrent daemon instances. |
| `$DATA_DIR/bot_secret_token` (planned) | 256-bit hex HTTP secret (`0o600`). |

## Runtime/Tooling Preferences

- **Language:** Rust.
- **Build tool:** Cargo; standalone crate, not a member of the Pacto workspace.
- **Async runtime:** Tokio.
- **HTTP framework:** axum (for the optional localhost HTTP transport).
- **Persistence:** SQLite via `rusqlite` (bundled), WAL mode.
- **Logging:** `tracing` / `tracing-subscriber`.
- **CLI:** `clap`.
- **Planned key dependencies:** `nostr-sdk` 0.43, `tokio`, `serde`/`serde_json`, `rusqlite`, `toml`, `axum`, `tokio-util`, `tracing`, `clap`, `zeroize`, `uuid`, `secrecy`.
- **Dev/test tools:** `schemars`, `jsonschema`, `proptest`, `cargo-deny`.
- **External services required for integration testing:** local Nostr relay (`ws://localhost:7000`), local Anvil EVM node (`http://localhost:8545`), optional NIP-46 bunker.
- **No Node.js in this crate.** The broader Pacto ecosystem uses pnpm/Node 20, but the daemon is Rust-only.

## Testing & QA

- **Default test mode:** `cargo test` runs in-process against mock relay and mock bunker implementations in `tests/support/`. Target: under 30 seconds, no Docker.
- **Integration mode:** gated tests against `pacto-dev-env` Docker services (`cargo test -- --ignored` with `PACTO_DEV_ENV=1`).
- **Property/chaos tests:** `proptest` for frame parsing, rate limiting, cursor advancement, and handler authorization.
- **Schema sync:** `schemas/` JSON Schema/OpenRPC artifacts are canonical; CI enforces that generated Rust types stay in sync.
- **Secret-redaction suite:** dedicated tests inject synthetic secrets into every log sink, error path, and binary string, asserting no leakage.
- **Requirement traceability:** plan references requirements R1–R37; changes should update traced coverage where the project enforces it.
- **Linting:** clippy (with custom lints forbidding plain strings for secrets) and `cargo-deny` for audit gates.

## Notes for AI Assistants

- Do not assume a `src/` directory exists yet. Before editing code, verify whether scaffolding has been created.
- Respect the planned separation of concerns: runtime logic belongs in the daemon, lifecycle/identity operations belong in `pacto-bot-admin`.
- When generating config examples, enforce `0o600` permissions and warn against committing real nsec values.
- Prefer deterministic, Docker-free tests; gate external-service tests behind `#[ignore]`.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->

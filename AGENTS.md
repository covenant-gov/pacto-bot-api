# Repository Guidelines

## Project Overview

`pacto-bot-api` is a standalone Rust daemon that multiplexes multiple Pacto bot identities onto one shared backend. Bot developers write handlers in any language and connect to the daemon over a language-agnostic JSON-RPC 2.0 API; the daemon owns Nostr relay connections, encrypted DM handling, signing keys, and message routing.

> **Current state:** Active implementation. The daemon, admin CLI, JSON-RPC transports, SQLite persistence, NIP-46 signing, handler dispatch, and bot project scaffolding are all implemented. The crate is currently at version **0.9.0**. See `CHANGELOG.md` for release history and `docs/plans/` for upcoming work.

## Architecture & Data Flow

```
pacto-bot-api daemon (Rust/Tokio)
├── ClientManager      # one BotState per configured bot identity
├── HandlerRegistry    # active handler_id → connection + capabilities
├── Dispatch           # fan-out by event type + bot npub
├── Transport Layer
│   ├── Unix socket    # $DATA_DIR/pacto-bot-api.sock, 0o600
│   └── localhost HTTP # 127.0.0.1:9800, X-Pacto-Bot-Secret
├── nostr-sdk Client   # shared relay pool + subscriptions
├── NIP-46 bunkers     # one signing connection per bot identity
└── SQLite (rusqlite)  # $DATA_DIR/agent.db — cursors, handlers, config, diagnostics

pacto-bot-admin CLI
├── new                # create bot identity; --scaffold generates a handler project
├── scaffold           # scaffold a handler project for an existing bot identity
├── publish-profile    # publish kind:0 metadata
├── test-bunker        # verify bunker connectivity and npub match
├── export / import    # move daemon-local state between data dirs
├── validate-config
├── rotate-http-token
├── diagnose / status
└── --llm-help         # print the generated operator guide
```

**Flow:**
1. The daemon reads static bot identities from `pacto-bot-api.toml`.
2. `ClientManager` creates one `BotState` per identity, connecting to relays and the configured signing backend.
3. Incoming `kind:1059` gift wraps are decrypted and forwarded as `agent.event` notifications to matching handlers.
4. Handlers reply via `agent.send_dm` / `agent.set_profile` / `agent.error`; the daemon verifies capabilities per-call, encrypts/wraps, and publishes.

Key pattern: **daemon manages runtime, admin CLI manages lifecycle**. The daemon never creates or deletes bot identities; `pacto-bot-admin` creates keys, publishes profiles, tests bunkers, scaffolds handler projects, and exports/imports state.

## Key Directories

| Path | Purpose |
|------|---------|
| `src/` | Daemon, admin CLI, transports, dispatch, persistence, signer, diagnostics, and scaffold generator. |
| `src/transport/` | JSON-RPC framing, Unix socket, and HTTP transports. |
| `src/scaffold/` | Template-driven bot project generator used by `pacto-bot-admin new --scaffold` and `scaffold`. |
| `src/*_generated.rs` | Rust types generated from `schemas/` by `cargo xtask codegen`. Do not hand-edit. |
| `tests/` | In-process integration tests, mock relay/bunker support, fixtures, and contract tests. |
| `tests/support/` | Shared mock relay, mock bunker, secret scanner, and test helpers. |
| `schemas/` | Canonical JSON Schema/OpenRPC contracts; source of truth for generated types. |
| `docs/solutions/` | Documented solutions to recurring problems and review patterns (searchable via YAML frontmatter). |
| `xtask/` | Build/task runner (`codegen`, `docs`, `coverage`, `secret-lint`, `dev-env-probe`). |
| `tests/fixtures/templates/` | Local cargo-generate fixture template used by the integration tests. |
| `skills/` | Installable skill files for `npx skills` (project-local skills such as `kind-lookup` and `nip-lookup`; `python-pacto-bot` is provided by the `pacto-bot-templates` repository). |
| `python/` | Generated Python SDK (`pacto-bot-sdk`), reference bots in `python/examples/`, and manifest-driven contract tests in `python/tests/`. |
| `scripts/` | Release install script, packaging script, and pre-commit hook. |
| `.github/workflows/` | CI and release automation. |
| `.config/nextest.toml` | `cargo-nextest` profile used by `make test-fast`. |
| `docs/` | Architecture research, implementation plans, setup guides, and the generated `pacto-bot-admin-llms.txt`. |
| `docs/plans/` | Formal feature plans and security reviews. |
| `data/` | Default runtime data directory (gitignored). |

## Development Commands

```bash
# Format and clippy validation gate (not the full test suite) — run this before committing
make validate

# Build all targets
make build

# Build and run the daemon
cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml --data-dir ./data

# Run the admin CLI
cargo run --bin pacto-bot-admin -- new my-bot
cargo run --bin pacto-bot-admin -- new --scaffold my-bot --backend nsec --relays ws://localhost:7000 --commands echo
cargo run --bin pacto-bot-admin -- scaffold my-bot --commands echo
cargo run --bin pacto-bot-admin -- publish-profile my-bot
cargo run --bin pacto-bot-admin -- test-bunker my-bot
cargo run --bin pacto-bot-admin -- diagnose --format json
cargo run --bin pacto-bot-admin -- status --format json

# Default test suite (in-process mocks, no Docker)
make test        # reliable sequential run, ~76s
make test-fast   # parallel run with cargo-nextest, ~20-25s

# Gated integration tests against pacto-dev-env (Docker)
cargo test -- --ignored

# Regenerate Rust types from schemas/
cargo xtask codegen

# Regenerate the LLM operator guide
cargo xtask docs

# Cross-compilation (requires zig + cargo-zigbuild)
make cross-compile-macos   # macOS x86_64 + arm64
make cross-compile-linux   # Linux x86_64 + arm64 musl
make cross-compile-windows # Windows x86_64
make cross-compile-freebsd # FreeBSD x86_64
make package               # build and package all release artifacts

# Other helpers
make coverage
make deny
make install-hooks
```

**Always run `make validate` before committing.** It runs `cargo fmt --check` and `cargo clippy`. Do not commit code that fails this gate.

Ecosystem-wide setup for local services (relay, EVM testnet, bunker):

```bash
cd dev-setup             # in a repo that provides it (pacto-app or pacto-dev-env)
docker compose up -d --build
docker compose --profile bunker up -d --build
```

## Code Conventions & Common Patterns

### Language & style
- Rust with Tokio async runtime; edition 2024; MSRV 1.96.
- Use `snake_case` for JSON-RPC method/field names; Rust structs use `PascalCase` with `serde(rename_all = "snake_case")`.
- Two binary targets: `pacto-bot-api` (daemon) and `pacto-bot-admin` (CLI).
- Generated Rust types live in `src/*_generated.rs` and are produced from `schemas/` by `cargo xtask codegen`. Do not hand-edit generated files.

### Error handling
- Use standard Rust `Result` propagation; avoid panics for operational errors (config validation, relay failure, bunker mismatch).
- Errors returned to handlers are JSON-RPC 2.0 error objects; secrets must never appear in error messages.
- Custom lint group in `Cargo.toml` denies `unwrap_used`, `expect_used`, `panic`, and `disallowed_names` (e.g., `foo`, `bar`, `baz` placeholders) across non-test code.

### Secrets & cryptography
- Represent nsec, bunker URIs, and the HTTP secret token with `secrecy::SecretString` or `zeroize::Zeroizing`.
- `nsec` backend clears key material on drop with `zeroize`; still treated as dev-only.
- Never log secrets, config signing material, or the HTTP token.
- Config files and runtime secret files must be `0o600` or stricter; the daemon and CLI enforce this.

### Async & state management
- `ClientManager` owns per-bot `BotState` (npub, relay subscriptions, bunker connection).
- `HandlerRegistry` owns active registrations and routing.
- SQLite in WAL mode persists cursors, handler registrations, diagnostics, and config; cursor advancement waits for terminal handler responses or dispatch timeout.

### Dependency injection
- Constructor injection for testability. Mock relay and mock bunker implementations live in `tests/support/` for the default test suite.

### Capability & authorization
- Handlers register for specific bot identities and capabilities.
- Every mutating call (`agent.send_dm`, `agent.set_profile`, `agent.error`) is authorized against the registration, not just at connection time.

### Common review feedback patterns

The following patterns recur in PR review and should be applied proactively so that security, performance, and protocol-compliance issues are caught before review:

- **Secure file creation.** Files containing sensitive data (diagnostics, secrets, DB, etc.) must be created with explicit owner-only permissions. Do not rely on umask or apply permissions after a rename. Prefer `tokio::fs::OpenOptions::new().write(true).create(true).mode(0o600)` on Unix. On Windows, create the file with the owner-only DACL already in place via `SECURITY_ATTRIBUTES`; do not create with the inherited ACL and tighten afterward. Also remove the temp file on any failure in a temp-then-rename workflow. See `docs/solutions/best-practices/secure-file-creation.md`.
- **JSON-RPC error code allocation.** Before adding or changing a JSON-RPC error code, check the plan's error-code table (`docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`) and the existing mapping in `src/errors.rs`. Pick the next unused code in the `-32000` to `-32099` range; do not reuse an existing code because the message differs. Prefer routing through `DaemonError`; if a transport-level error needs a custom code, document it in the plan. See `docs/solutions/best-practices/json-rpc-error-codes.md`.
- **Regex compilation in hot paths.** Do not compile `regex::Regex` inside functions that are called repeatedly (including error paths). Cache compiled regexes with `std::sync::LazyLock` (or `OnceLock`/`once_cell`) so they are built once per process. See `src/diagnostics.rs` (`redact_secrets`) for a working example.
- **Amortize cleanup in hot paths.** Avoid O(n) scans or full-map cleanup on every request. Gate sweeps on a size threshold or a time-based cadence tracked in the data structure. See `src/dispatch.rs` (`RateLimiter`/`BucketMap`) for a working example.
- **Exact test assertions for exact contracts.** When the protocol or API specifies an exact value (e.g., `Content-Type: application/json; charset=utf-8`), assert the exact value rather than a prefix or substring. Prefix/substring checks are only appropriate when the contract is intentionally loose. See `tests/transport_http.rs` (`assert_json_content_type`) for a working example.

See also `docs/solutions/` for documented solutions and patterns in these areas.

### Generated code workflow
- `schemas/` is the source of truth. Run `cargo xtask codegen` to regenerate `src/*_generated.rs`.
- `tests/schema_sync.rs` enforces that generated files are in sync with schemas; CI fails if they drift.

## Important Files

| File | Purpose |
|------|---------|
| `Cargo.toml` | Package manifest, workspace definition, lints, and dependency set. Current version is 0.9.0. |
| `Makefile` | Development shortcuts including validation, cross-compilation, packaging, and hooks. |
| `pacto-bot-api.toml` | Runtime daemon config (gitignored in production; example in repo root or docs). |
| `README.md` | Operator-facing quickstart and installation guide. |
| `CHANGELOG.md` | Release history and unreleased changes. |
| `docs/pacto-bot-admin-llms.txt` | Generated LLM-readable operator guide; regenerate with `cargo xtask docs`. |
| `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` | Primary implementation plan: requirements, architecture, JSON-RPC catalog, config schema, security invariants. |
| `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-executive-summary.md` | High-level concept and Phase 1 scope. |
| `docs/plans/2026-06-24-001-security-review-findings.md` | Security findings and resolutions. |
| `docs/plans/2026-06-30-001-feat-bot-scaffold-plan.md` | Plan for the `pacto-bot-admin` bot project scaffold generator. |
| `docs/GETTING_STARTED.md` | Ecosystem-wide local dev setup. |
| `docs/security-overview.md` | Security model and threat considerations. |
| `docs/key-and-secret-security.md` | Key handling and secret hygiene. |
| `schemas/jsonrpc.json` | OpenRPC/JSON Schema contract for handler-facing methods. |
| `schemas/config.json` | JSON Schema for `pacto-bot-api.toml`. |
| `schemas/example-manifest.json` | Manifest for validating example Python bots. |
| `$DATA_DIR/agent.db` | SQLite persistence. |
| `$DATA_DIR/daemon.lock` | Exclusive lock preventing concurrent daemon instances. |
| `$DATA_DIR/bot_secret_token` | 256-bit hex HTTP secret (`0o600`). |

## Runtime/Tooling Preferences

- **Language:** Rust.
- **Build tool:** Cargo; standalone crate with `xtask` workspace member.
- **Async runtime:** Tokio.
- **HTTP framework:** axum (optional localhost HTTP transport).
- **Persistence:** SQLite via `rusqlite` (bundled), WAL mode.
- **Logging:** `tracing` / `tracing-subscriber`.
- **CLI:** `clap`.
- **Key runtime dependencies:** `nostr-sdk` 0.43, `tokio`, `serde`/`serde_json`, `rusqlite`, `toml`, `axum`, `tokio-util`, `tracing`, `clap`, `zeroize`, `uuid`, `secrecy`, `thiserror`, `chrono`, `fs2`, `tokio-tungstenite`, `hex`, `bech32`, `subtle`, `reqwest`.
- **Dev/test tools:** `schemars`, `jsonschema`, `proptest`, `assert_cmd`, `predicates`, `parking_lot`, `nix`, `futures`, `syn`, `quote`.
- **External services required for integration testing:** local Nostr relay (`ws://localhost:7000`), local Anvil EVM node (`http://localhost:8545`), optional NIP-46 bunker.
- **Packaging:** `cargo-zigbuild` for cross-compilation, Docker multi-stage image, GHCR publish via CI, `scripts/package-release.sh` for tar/zip + SHA-256 artifacts.
- **No Node.js in this crate.** The broader Pacto ecosystem uses pnpm/Node 20, but the daemon is Rust-only. The Python SDK and examples use Python 3.

## Testing & QA

- **Default test mode:** `make test` runs the full suite sequentially with `cargo test` against mock relay and mock bunker implementations in `tests/support/`. `make test-fast` runs the same tests in parallel with `cargo-nextest` (~20–25 s). Target: under 30 seconds, no Docker.
- **Integration mode:** gated tests against `pacto-dev-env` Docker services (`cargo test -- --ignored` with `PACTO_DEV_ENV=1`).
- **Test temp directories:** integration tests use `common::tempdir()` from `tests/common/mod.rs` instead of `tempfile::tempdir()`. This places temp directories under `target/test-temp` with `0o700` permissions so daemon config-file permission checks pass regardless of the host `/tmp` permissions.
- **Property/chaos tests:** `proptest` for frame parsing, rate limiting, cursor advancement, and handler authorization.
- **Schema sync:** `schemas/` JSON Schema/OpenRPC artifacts are canonical; CI enforces that generated Rust types stay in sync.
- **Secret-redaction suite:** dedicated tests inject synthetic secrets into every log sink, error path, and binary string, asserting no leakage.
- **Requirement traceability:** plan references requirements R1–R37; changes should update traced coverage where the project enforces it.
- **Linting:** clippy (with custom lints forbidding plain strings for secrets, `unwrap`, `expect`, and `panic` in production code) and `cargo-deny` for audit gates.

## Agent Skills

This repository vendors agent skills so contributors working in Claude Code, Cursor, or Oh My Pi get consistent Rust guidance without installing the skills CLI themselves.

### Layout

| Path | Purpose |
|---|---|
| `.claude/skills/` | Claude Code skill provider |
| `.agents/skills/` | Cursor and OMP shared provider |
| `.omp/skills/` | Oh My Pi native provider |
| `skills-lock.json` | Reproducible skill manifest |

Skills are installed with `npx skills add ... --copy` so the files are committed to the repo.

### Installed skills

| Skill | Source | Purpose |
|---|---|---|
| `rust-best-practices` | `apollographql/skills` | Idiomatic Rust, ownership, error handling, performance, linting |
| `rust-async-patterns` | `wshobson/agents` | Tokio, async traits, concurrency, async debugging |
| `rust-testing` | `affaan-m/everything-claude-code` | Unit, integration, async, property-based, and snapshot testing |
| `rust-patterns` | `affaan-m/everything-claude-code` | Common Rust design patterns |
| `m15-anti-pattern` | `zhanghandong/rust-skills` | Anti-patterns and code-smell detection |
| `cargo-fuzz` | `trailofbits/skills` | Fuzzing with `cargo-fuzz` / `libFuzzer` |
| `cargo-nextest` | `laurigates/claude-plugins` | Fast, structured test runs with `cargo nextest` |
| `ce-compound` | `everyinc/compound-engineering-plugin` | Document solved problems and project vocabulary in `docs/solutions/` |
| `ce-compound-refresh` | `everyinc/compound-engineering-plugin` | Audit and refresh stale learnings against the codebase |
| `python-pacto-bot` | `covenant-gov/pacto-bot-templates` | Write Python bots for `pacto-bot-api` using the generated SDK; directs new projects to `pacto-bot-admin new --scaffold` |
| `nip-lookup` | `project-local` | Look up a NIP and explain the 5 Ws / How plus Pacto-specific use cases |
| `kind-lookup` | `project-local` | Look up a Nostr event kind and explain the 5 Ws / How plus Pacto-specific use cases |

### Security note

`cargo-fuzz` is flagged as higher-risk by skills.sh because fuzzing invokes compilers and runs arbitrary generated inputs. The skill is from Trail of Bits, a reputable security firm, and should be reviewed before use on sensitive code paths. Do not run fuzzing against production secrets or live services.

## Notes for AI Assistants

- `src/` and `tests/` exist. Use `grep` and `ast_grep` to find code; do not assume the repo is planning-only.
- **Always run `make validate` before committing.** It runs `cargo fmt --check` and `cargo clippy`. Do not commit code that fails this gate.
- Respect the planned separation of concerns: runtime logic belongs in the daemon, lifecycle/identity operations belong in `pacto-bot-admin`, and bot authoring belongs in the Python SDK / scaffold generator.
- When generating config examples, enforce `0o600` permissions and warn against committing real nsec values.
- Prefer deterministic, Docker-free tests; gate external-service tests behind `#[ignore]`.
- Do not hand-edit `src/*_generated.rs` files; update `schemas/` and run `cargo xtask codegen` instead.
- When adding new admin CLI commands, include per-command `after_help` examples and update `docs/pacto-bot-admin-llms.txt` via `cargo xtask docs`.
- **When changing the `capabilities` description in `schemas/jsonrpc.json`:** the Python SDK generator copies that description into generated `HandlerRegisterParams` docstrings and into `python/tests/test_generator.py::HANDLER_REGISTER_PARAMS_SNAPSHOT`. Run the full Python test suite (`cd python && source .venv/bin/activate && pytest tests/`) after any schema description change that affects capabilities, and update the snapshot string.
- **When adding a new JSON-RPC method that mutates state:** add it to the hand-written `Method` enum and `FromStr`/`all()` lists in `src/transport/protocol.rs`, and make sure it is gated as mutating in HTTP transport if it changes daemon state.
- **When regenerating the Python SDK:** verify that generated `__all__` exports in `python/src/pacto_bot_sdk/_generated/models.py` include any new request/response models, and run the full Python test suite to catch snapshot or contract drift.
- **Before declaring a Python-only change safe:** run `pytest tests/` inside the Python venv. Do not rely only on `make validate`, which does not exercise Python tests.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:970c3bf2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands, and see `docs/BEADS_WORKFLOW.md` for the developer onboarding guide, daily workflow, and troubleshooting.

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
   bd dolt push
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

# Development guide

This document covers how to build, test, and contribute to `pacto-bot-api`.

## Prerequisites

- Rust toolchain 1.85 or later ([rustup](https://rustup.rs/))
- A POSIX shell for scripts and examples
- Python 3.10+ if running the reference handler/examples tests

## Build

```bash
cargo build
```

Release build:

```bash
cargo build --release
```

## Run checks

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo nextest run
cargo deny check
```

Project tooling:

- `.clippy.toml` — project-specific Clippy lints (e.g., forbidding plain `String`/`&str` for secrets).
- `deny.toml` — license and audit policy for `cargo-deny`.
- `xtask/` — project automation such as schema/codegen tasks (`cargo xtask codegen`).

## Git hooks

A pre-commit hook is available in `scripts/pre-commit.sh`. It runs the Beads pre-commit hook (if installed) and `make validate` (format check, clippy, and tests).

Install it:

```bash
cp scripts/pre-commit.sh .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
```

## Running the daemon

```bash
cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml
```

Flags:

- `--config <PATH>` — path to the TOML config (default: `pacto-bot-api.toml`)
- `--data-dir <DIR>` — override the runtime data directory
- `--enable-http` — start the optional localhost HTTP transport

The daemon requires the config file to be `0o600` or stricter.

## Running the admin CLI

```bash
# Create a new bot identity
cargo run --bin pacto-bot-admin -- new echo-bot --backend nsec

# Publish a kind:0 bot profile
cargo run --bin pacto-bot-admin -- publish-profile echo-bot

# Verify a bunker connection
cargo run --bin pacto-bot-admin -- test-bunker echo-bot

# Validate config
cargo run --bin pacto-bot-admin -- validate-config

# Rotate the HTTP secret token
cargo run --bin pacto-bot-admin -- rotate-http-token

# Structured diagnostics
cargo run --bin pacto-bot-admin -- diagnose --format json
```

## Test modes

### Default: in-process, no Docker

```bash
cargo nextest run
```

This runs the full default suite using in-process mock relay and mock bunker implementations. Target: under 30 seconds.

If you do not have `cargo-nextest` installed, the standard test runner works as a fallback:

```bash
cargo test
```

### Integration tests

Integration tests live in `tests/` and run against in-process mock relay and bunker implementations by default. Run them with:

```bash
cargo nextest run --test integration
```

Tests that need external services (a local Nostr relay, EVM node, or NIP-46 bunker) are gated behind `#[ignore]` and can be run selectively once those services are available:

```bash
cargo nextest run --run-ignored all
```

### Schema sync

```bash
cargo xtask codegen
cargo test --test schema_sync
```

The canonical API contract lives in `schemas/`. Rust types are generated from these schemas, and `tests/schema_sync.rs` ensures they stay in sync.

## Configuring a bot

Copy the example config:

```bash
cp pacto-bot-api.toml.example pacto-bot-api.toml
chmod 0o600 pacto-bot-api.toml
```

A minimal single-bot config:

```toml
[daemon]
data_dir = "~/.local/share/pacto-bot-api"
socket_path = "~/.local/share/pacto-bot-api/pacto-bot-api.sock"

[[bots]]
id = "echo-bot"
npub = "npub1..."
signing = { backend = "nsec", nsec = "${PACT_BOT_NSEC}" }
relays = ["wss://relay.pacto.chat"]
capabilities = ["ReadMessages", "SendMessages"]
```

- `id` must be unique within the config.
- `nsec` supports `${ENV_VAR}` expansion; never commit a raw nsec.
- `bunker_remote` URIs must use `wss://`.

## Code conventions

- Rust edition 2024.
- JSON-RPC method/field names use `snake_case`; Rust structs use `PascalCase` with `serde(rename_all = "snake_case")`.
- Secrets are represented with `secrecy::SecretString` or `zeroize::Zeroizing`; plain `String`/`&str` for secrets is forbidden by clippy lints.
- Keep `main.rs` and `admin.rs` thin; business logic lives in modules.

## Useful commands

```bash
# Watch and run tests on change
cargo watch -x "nextest run"

# Run a specific test binary
cargo nextest run --test cli_args

# Run a single test by name
cargo nextest run --test cli_args -- my_test_name

# Generate and view docs
cargo doc --open
```

## Adding a new JSON-RPC method

1. Update `schemas/jsonrpc.json`.
2. Run `cargo xtask codegen`.
3. Add the handler in `src/transport/protocol.rs` or `src/dispatch.rs`.
4. Add a test referencing the requirement(s), e.g. `#[req(R15)]`.

## Troubleshooting

### Config permission error

```text
failed to load config: config file permissions are too permissive
```

Fix:

```bash
chmod 0o600 pacto-bot-api.toml
```

### Lock file already held

The daemon uses `$DATA_DIR/daemon.lock` to prevent concurrent instances. If a crash leaves a stale lock, remove it only when you are certain no daemon is running:

```bash
rm ~/.local/share/pacto-bot-api/daemon.lock
```

### `nsec` not found

If using `signing = { backend = "nsec", nsec = "${PACT_BOT_NSEC}" }`, ensure the environment variable is exported in the daemon's environment.

## Getting help

- Architecture background: [`docs/pacto-bot-architecture-deep-dive-2.md`](docs/pacto-bot-architecture-deep-dive-2.md)
- Implementation plan: [`docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`](docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md)
- Open a Beads issue: `bd create --title="..." --description="..." --type=bug`

# pacto-bot-api

A standalone Rust daemon that multiplexes multiple Pacto bot identities onto one shared backend and exposes a language-agnostic JSON-RPC 2.0 API.

## The 5 Ws

| Question | Answer |
|----------|--------|
| **What** | A daemon plus admin CLI that owns Nostr relay connections, encrypted DM handling, signing keys, and message routing for one or more Pacto bots. |
| **Who** | Bot operators run the daemon; bot developers write handlers in any language that speak JSON-RPC over a Unix socket or localhost HTTP. |
| **Why** | Running one daemon amortizes the heavy Pacto backend (nostr-sdk, MLS engine, RPC, SQLite) across all bots instead of duplicating it per bot. |
| **Where** | Self-hosted by each operator — typically `~/.local/share/pacto-bot-api` on a server or workstation. |
| **When** | Phase 1 supports multi-bot static config, NIP-17/44/59 DMs, local test keys, NIP-46 bunkers, and handler registration. |

## Quickstart

### 1. Install

#### Install from a GitHub release

The fastest way to get the daemon and admin CLI is to use the release install
script. It detects your platform (macOS or Linux) and architecture (x86_64 or
arm64), downloads the latest GitHub release, verifies the SHA-256 checksum, and
installs both binaries into `/usr/local/bin`:

```bash
curl -sSL https://raw.githubusercontent.com/logicminds/pacto-bot-api/main/scripts/install.sh | bash
```

You can customize the installation with environment variables:

```bash
# Install to ~/.local/bin instead of /usr/local/bin
curl -sSL https://raw.githubusercontent.com/logicminds/pacto-bot-api/main/scripts/install.sh | INSTALL_PREFIX=~/.local bash

# Install a specific version instead of latest
curl -sSL https://raw.githubusercontent.com/logicminds/pacto-bot-api/main/scripts/install.sh | PACTO_VERSION=0.1.0 bash
```

#### Build from source

Requires Rust 1.85 or later.

```bash
git clone https://github.com/covenant-gov/pacto-bot-api
cd pacto-bot-api
cargo build --release
```

See [`BUILDING.md`](BUILDING.md) for cross-compilation instructions (macOS, Linux, Windows; x86_64 and arm64).

Binaries:

- `target/release/pacto-bot-api` — the daemon
- `target/release/pacto-bot-admin` — lifecycle/admin CLI

### 2. Create a bot identity

```bash
pacto-bot-admin new echo-bot --backend nsec
```

This prints an `npub`, an `nsec`, and a `[[bots]]` config snippet. For anything beyond local experimentation, use a NIP-46 bunker instead of `nsec`.

If you built from source, use `cargo run --bin pacto-bot-admin -- new echo-bot --backend nsec` instead.

### 3. Configure the daemon

```bash
cp pacto-bot-api.toml.example pacto-bot-api.toml
chmod 0o600 pacto-bot-api.toml
```

Paste the snippet from `pacto-bot-admin new` into `pacto-bot-api.toml`, set the `nsec` via the `PACT_BOT_NSEC` environment variable, and adjust `relays` as needed.

### 4. Run the daemon

```bash
PACT_BOT_NSEC=<nsec-hex> pacto-bot-api --config pacto-bot-api.toml
```

Add `--enable-http` to start the optional localhost HTTP transport on `127.0.0.1:9800`.

If you built from source, use `cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml` instead.

### 5. Connect a handler

The easiest way to write a handler is with the generated Python SDK in
[`python/`](python/). It handles JSON-RPC framing, transport selection,
registration, command dispatch, and response shaping:

```python
from pacto_bot_api import Bot

bot = Bot(bot_id="echo-bot")


@bot.command("/echo")
async def echo(event, bot):
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": event.content.removeprefix("/echo ").strip(),
    }


@bot.default
async def unknown(event, bot):
    return {"event_id": event.event_id, "action": "ignore"}


if __name__ == "__main__":
    bot.run()
```

Save it as `echo_bot.py` and run it against the daemon's Unix socket:

```bash
pip install -e python/
python echo_bot.py --socket ~/.local/share/pacto-bot-api/pacto-bot-api.sock
```

Handlers can also connect directly over the Unix socket or HTTP transport and
speak JSON-RPC 2.0 themselves. The canonical API contract lives in
[`schemas/`](schemas/). A raw registration request looks like:

```json
{"jsonrpc":"2.0","id":1,"method":"handler.register","params":{"bot_ids":["echo-bot"],"event_types":["dm_received"],"capabilities":["ReadMessages","SendMessages"]}}
```

Incoming DMs arrive as `agent.event` notifications; handlers reply with
`agent.send_dm` or `handler.response`.

Reference material:

- [`python/README.md`](python/README.md) — full Python SDK guide (`Bot`,
  `PactoClient`, capabilities, transports, all examples).
- [`docs/python-sdk.md`](docs/python-sdk.md) — SDK overview and regeneration
  notes.
- [`python/examples/greeting_bot.py`](python/examples/greeting_bot.py) and
  [`python/examples/joke_bot.py`](python/examples/joke_bot.py) — reference bots
  using the generated SDK.
- [`examples/`](examples/) — legacy standard-library seed handler (`echo_bot.py`)
  and pytest fixtures/tests.
- [`tests/example_http_handler.rs`](tests/example_http_handler.rs) and
  [`tests/example_multi_bot.rs`](tests/example_multi_bot.rs) — Rust example tests.

## Repository layout

```text
pacto-bot-api/
├── Cargo.toml                 # Rust crate manifest
├── pacto-bot-api.toml.example # Example daemon config
├── README.md                  # This file
├── DEVELOPMENT.md             # Contributor and development guide
├── BUILDING.md                # Native and cross-compilation instructions
├── schemas/                   # Canonical JSON Schema / OpenRPC contracts
├── src/                       # Daemon and admin CLI source
├── tests/                     # In-process integration tests
├── python/                    # Generated Python SDK and Python tests
│   ├── src/pacto_bot_api/     # SDK package (`Bot`, `PactoClient`, models)
│   ├── examples/              # Reference bots using the generated SDK
│   └── tests/                 # Python SDK tests
├── examples/                  # Legacy standard-library seed handler/tests
└── xtask/                     # Build/task runner (cargo xtask codegen)
```

## Security notes

- The config file must be `0o600` or more restrictive; the daemon refuses to start otherwise.
- The `nsec` backend is a dev-only convenience. Production bots must use a NIP-46 bunker.
- The Unix socket is created with `0o600`; any process running as the daemon user can connect.
- The HTTP transport is disabled by default. When enabled, it requires `X-Pacto-Bot-Secret`.
- Secrets (nsec, bunker URI, HTTP token) are never logged or returned in error responses.

## Status

Phase 1 of the daemon is implemented and passes its in-process test suite:

- Multi-bot static config loaded from `pacto-bot-api.toml`.
- Full daemon event loop with Unix-socket and optional localhost HTTP transports.
- NIP-17/44/59 DM send/receive over a shared `nostr-sdk` relay pool.
- Three signing backends: dev-only `nsec`, local NIP-46 bunker, and remote NIP-46 bunker.
- Handler registration, capability enforcement, fan-out dispatch, and per-handler/per-bot rate limits.
- SQLite persistence with WAL mode, cursor recovery, and `export`/`import` via `pacto-bot-admin`.
- Structured diagnostics, metrics, last-run reports, and a schema-first contract in `schemas/`.
- Docker-free integration tests using in-process mock relay and bunker implementations.
- Generated Python SDK in `python/` with typed models, `PactoClient`, and a decorator-based `Bot` API.

Phase 2 and beyond (MLS group participation, on-chain governance reads/writes, webhook delivery) are planned but not yet implemented.

See [`docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`](docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md) for the full implementation plan and roadmap.

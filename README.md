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
installs the workspace binaries into `/usr/local/bin`:

```bash
curl -sSL https://raw.githubusercontent.com/covenant-gov/pacto-bot-api/main/scripts/install.sh | bash
```

You can customize the installation with environment variables:

```bash
# Install to ~/.local/bin instead of /usr/local/bin
curl -sSL https://raw.githubusercontent.com/covenant-gov/pacto-bot-api/main/scripts/install.sh | INSTALL_PREFIX=~/.local bash

# Install a specific version instead of latest
curl -sSL https://raw.githubusercontent.com/covenant-gov/pacto-bot-api/main/scripts/install.sh | PACTO_VERSION=0.10.0 bash
```

#### Build from source

Requires Rust 1.96 or later.

```bash
git clone https://github.com/covenant-gov/pacto-bot-api
cd pacto-bot-api
cargo build --release
```

See [`BUILDING.md`](BUILDING.md) for cross-compilation instructions (macOS, Linux, Windows; x86_64 and arm64).

Binaries:

- `target/release/pacto-bot-api` — the daemon
- `target/release/pacto-bot-admin` — lifecycle/admin CLI
- `target/release/create-mls-group` — MLS group creation utility

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

Paste the snippet from `pacto-bot-admin new` into `pacto-bot-api.toml`, set the `nsec` via the `PACTO_BOT_NSEC` environment variable, and adjust `relays` as needed.

Attachment settings are optional, so an unmodified 0.8.0 config remains valid. The defaults are equivalent to:

```toml
[daemon]
attachment_max_bytes = 10485760            # 10 MiB, inbound and outbound
spool_outbound_retention_secs = 86400      # failed/abandoned outbound files
blob_servers = ["https://nostr.download"] # ordered Blossom upload failover
```

Verified inbound plaintext remains in `$DATA_DIR/spool/inbound` for one hour.
Handlers stage larger outbound files under the `spool_dir` returned by
`handler.register` or `handler.reconnect`; successful sends remove the staged
file, while abandoned entries are swept after the configured retention period.

### 4. Run the daemon

```bash
PACTO_BOT_NSEC=<nsec-hex> pacto-bot-api --config pacto-bot-api.toml
```

Add `--enable-http` to start the optional localhost HTTP transport on `127.0.0.1:9800`.

If you built from source, use `cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml` instead.

### 5. Scaffold a Python handler project (optional)

The admin CLI can bootstrap a full bot handler project from an external
`cargo-generate` template repository instead of writing files by hand. First
install `cargo-generate`:

```bash
cargo install cargo-generate --version 0.23.0
```

Then scaffold a project:

```bash
pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo
```

> **Note:** The first scaffold may make unauthenticated GitHub API requests that are rate-limited to 60 requests per hour. Set `GITHUB_TOKEN` in your environment to use authenticated requests and avoid rate limits.

This resolves a compatible contract/SDK/template triple, caches the artifacts
locally, renders the project with `cargo-generate`, and writes a per-bot
`.pacto/bots/echo-bot/scaffold.lock` file so the project can be refreshed later.

To update an existing project from its lock file:

```bash
pacto-bot-admin update echo-bot
```

`update` protects user-edited files declared in the template's `manifest.toml`;
use `--force` to override them.

### 6. Connect a handler

The easiest way to write a handler is with the generated Python SDK, now
published to PyPI as `pacto-bot-sdk`:

```python
from pacto_bot_sdk import Bot

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
pip install pacto-bot-sdk
python python/examples/greeting_bot.py --socket ~/.local/share/pacto-bot-api/pacto-bot-api.sock
```

Handlers can also connect directly over the Unix socket or HTTP transport and
speak JSON-RPC 2.0 themselves. The canonical API contract lives in
[`schemas/`](schemas/). A raw registration request looks like:

```json
{"jsonrpc":"2.0","id":1,"method":"handler.register","params":{"bot_ids":["echo-bot"],"event_types":["dm_received"],"capabilities":["ReadMessages","SendMessages"]}}
```

Incoming content arrives as typed `agent.event` notifications. Subscribe to
`reaction_received`, `attachment_received`, `mls_group_reaction_received`, or
`mls_group_attachment_received` separately from text messages. Send operations
are `agent.send_reaction`, `agent.send_group_reaction`, `agent.send_attachment`,
and `agent.send_group_attachment`; each requires its matching capability.
Attachment send requests provide exactly one of a confined `spool_path` or
small standard-base64 `inline_base64` payload. The daemon owns MIME sniffing,
hashing, encryption, Blossom upload, and Nostr publication.

Reference material:

- [`python/README.md`](python/README.md) — full Python SDK guide (`Bot`,
  `PactoClient`, capabilities, transports, all examples).
- [`docs/python-sdk.md`](docs/python-sdk.md) — SDK overview and regeneration
  notes.
- [`python/examples/greeting_bot.py`](python/examples/greeting_bot.py) and
  [`python/examples/joke_bot.py`](python/examples/joke_bot.py) — reference bots
  using the generated SDK.
- [`tests/example_http_handler.rs`](tests/example_http_handler.rs) and
  [`tests/example_multi_bot.rs`](tests/example_multi_bot.rs) — Rust example tests.

## Upgrading to nostr 0.44 / MDK 0.8.0

This release moves the MLS engine from `mdk-*` 0.5.2 to 0.8.0 and requires a
per-bot store encryption key MDK now mandates. Every bot's existing MLS
groups need re-invitation after the upgrade. Follow this procedure in order:

1. **Back up `$DATA_DIR` before installing.** This is the only recovery path
   if a bot's MLS store fails closed after the upgrade — there is no other
   way back. Stop the daemon first so the backup is not taken mid-write.
2. **Install the new binaries and start the daemon** following
   [Install](#1-install) and [Run the daemon](#4-run-the-daemon) above.
3. **Run `pacto-bot-admin diagnose --format json`** and read, per bot:
   - `reset_at` — set when the daemon reset this bot's MLS store on a past
     start (missing key, wrong key, or a legacy pre-key store); absent means
     never reset.
   - `mls_groups[].state_held` — `false` means that group needs
     re-invitation (below); `true` needs nothing.
   - `error` — a non-null value starting with `"MLS engine unavailable: "`
     means the bot's MLS engine failed to construct after a fail-closed
     store classification; see step 6.
   - The sole-admin buckets — `repairable_now` (this bot still holds live
     state; run `mls-group repair-admins`, no external action needed),
     `unrestorable` (this bot was the squad's only admin; the squad must be
     re-created), and `admin_set_unknown` (the store was archived under an
     always-archived encrypted-store reset and its prior admin set cannot be
     recovered — treat as unrestorable).
4. **For every group with `state_held: false`, contact that Squad's admin**
   and ask them to re-invite the bot. There is no daemon-side action that
   recovers a state-lost group other than re-invitation — the archived
   credential is deliberately unrecoverable. For every `unrestorable` or
   `admin_set_unknown` group, re-create the Squad instead of waiting on a
   repair that cannot happen.
5. **What success looks like:** every bot's `reset_at` reflects a completed,
   expected reset (not a repeated reset on the next start); every group has
   `state_held: true` or a pending re-invitation request with its admin;
   `pacto-bot-admin mls-group repair-admins` has been run for every
   `repairable_now` entry, so no group the bot still holds state for stays
   sole-admin.
6. **If a bot fails closed** (its `error` field starts with `"MLS engine
   unavailable: "` in diagnose, or the daemon logs an MLS engine
   construction failure for that bot): restore `$DATA_DIR` from the step-1
   backup and re-run this procedure. There is no other recovery — the
   failed-closed classification exists specifically so the daemon never
   guesses at store contents it cannot verify.
7. **On suspected key or archive compromise** (the per-bot store key, or an
   archived legacy store under `mls_archive_retention_days`, may have been
   read by an unauthorized party): treat every Squad the affected bot
   belongs to as compromised — the key protects the store at rest, not the
   plaintext message content already inside it (see Security notes below).
   Rotate the bot's Nostr identity, have every Squad admin remove and
   re-create the bot in a fresh group, and delete the compromised store,
   key, and any archive under `$DATA_DIR` once every affected Squad has
   migrated off them.

**Security notes:**

- The per-bot store key lives beside the store as `<store-filename>.key`.
  Backing it up together with the store is intentional, but it also means
  **the encryption buys nothing against anyone who can already read
  `$DATA_DIR`** — it is not a substitute for filesystem and backup access
  control.
- The MLS store, and any archive of a legacy pre-upgrade store, hold the
  plaintext content of every group message the bot has decrypted. Enabling
  `mls_archive_retention_days` (default `0`, meaning no archive) retains that
  history, and any backup or file-sync tool watching `$DATA_DIR` will capture
  it.
- A rollback to a pre-upgrade binary can invalidate messages a handler
  already acted on, because MDK enforces stricter forward-secrecy bounds
  than the store previously carried.

## pacto-app interoperability check

Live interoperability is a release check because this repository does not bundle the
`pacto-app` GUI or a production Blossom host. Before publishing 0.10.0, run the daemon
and an unmodified current `pacto-app` against the same relay and project-operated blob
host, then verify both DM and Squad surfaces:

1. Have the bot send a reaction and an inline or spool-backed file; confirm the app
   attaches the reaction to the targeted message and downloads/decrypts the file.
2. From the app, react to a bot message and send a photo; confirm the handler receives
   the corresponding typed reaction/attachment event, the target id and emoji are
   correct, and the attachment path is readable with the expected plaintext hash.
3. Repeat in an MLS Squad, then confirm a handler subscribed only to text receives none
   of the reaction/attachment events.
4. Have the bot create a Squad and invite an app user; confirm the app decodes the
   bot's `kind:443` KeyPackage (base64 content, `encoding` tag) and joins from the
   resulting Welcome. Then have the app create a Squad and invite the bot; confirm the
   bot decrypts the app's Welcome and both directions exchange a message.
5. Simulate a restoration: after the bot's MLS store has been reset (or its group
   marked state-lost via `pacto-bot-admin diagnose`), have the Squad admin re-invite
   the bot with `pacto-bot-admin mls-group repair-admins` run first if the group is
   sole-admin. Confirm the bot resumes receiving and sending messages in that Squad
   afterward, and that other members' history is undisturbed.

Record the app commit, blob host, relay, event ids, and observed hashes in the release
run. Automated mock-relay/blob tests cover the same wire tags and crypto parameters,
but they do not replace this UI/runtime check.

## Debugging and observability

### Daemon logs

The daemon uses `tracing` and respects the standard `RUST_LOG` environment
variable. You can also pass `--log-level` on the daemon command line; when the
flag is set, it takes precedence over `RUST_LOG`.

```bash
# Show daemon debug logs
RUST_LOG=debug pacto-bot-api --config pacto-bot-api.toml

# Equivalent with the CLI flag
pacto-bot-api --config pacto-bot-api.toml --log-level debug
```

### Bot handler logs

Generated Python bots use the `pacto_bot_sdk` logger. Set `PACTO_LOG_LEVEL` to
control verbosity:

```bash
PACTO_LOG_LEVEL=debug python bots/echo-bot/echo_bot.py
```

Inside a Docker Compose stack, the variable is passed through automatically:

```bash
PACTO_LOG_LEVEL=debug docker compose up --build
```

### Health checks and quick fixes

`pacto-bot-admin doctor` checks the most common setup mistakes and prints
colored PASS/FAIL results with a fix suggestion for each failure:

```bash
pacto-bot-admin doctor
```

It validates the config file, data directory, daemon lock, configured bots,
relay reachability, registered handlers, and HTTP token permissions.

### End-to-end test tooling

Send a test DM from the daemon without involving a client and print the
resulting event ID:

```bash
pacto-bot-admin send-test-dm echo-bot npub1recipient... "hello"
```

The bot must have the `Admin` capability for this command to succeed.

Trace recent incoming events and outgoing replies for a bot:

```bash
pacto-bot-admin trace-events echo-bot
pacto-bot-admin trace-events echo-bot --since 30 --limit 50
```

Tail the daemon log file (if one exists):

```bash
pacto-bot-admin logs
pacto-bot-admin logs --follow
```

`pacto-bot-admin diagnose` includes recent event counts, reply-send failures,
per-bot cursors, and relay reachability in both text and JSON output.

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
├── python/                    # Generated Python SDK, examples, and tests
│   ├── src/pacto_bot_sdk/     # SDK package (`Bot`, `PactoClient`, models)
│   ├── examples/              # Reference bots using the generated SDK
│   └── tests/                 # Python SDK and example contract tests
└── xtask/                     # Build/task runner (cargo xtask codegen)
```

## Security notes

- The config file must be `0o600` or more restrictive; the daemon refuses to start otherwise.
- The `nsec` backend is a dev-only convenience. Production bots must use a NIP-46 bunker.
- The Unix socket is created with `0o600`; any process running as the daemon user can connect.
- The HTTP transport is disabled by default. When enabled, it requires `X-Pacto-Bot-Secret`.
- Secrets (nsec, bunker URI, HTTP token, attachment key/nonce, and decrypted payload content) are never logged or returned in error responses.
- `$DATA_DIR/spool` contains decrypted attachment plaintext. Keep it owner-only and exclude it from backups, cloud sync, indexing, and support bundles.
- Attachment event subscriptions expose readable local plaintext paths and are elevated privilege. Register only handlers that need file access, even though receive subscriptions do not require a send capability.
- Blob hosts see the bot's upload authorization npub, ciphertext size/hash, source IP, and timing, but cannot decrypt the payload from the upload alone.

## Status

Phase 1 of the daemon is implemented and passes its in-process test suite:

- Multi-bot static config loaded from `pacto-bot-api.toml`.
- Full daemon event loop with Unix-socket and optional localhost HTTP transports.
- NIP-17/44/59 DM send/receive over a shared `nostr-sdk` relay pool.
- Typed Nostr kind:7 reactions and encrypted kind:15 attachments on DM and MLS Squad surfaces.
- Three signing backends: dev-only `nsec`, local NIP-46 bunker, and remote NIP-46 bunker.
- Handler registration, capability enforcement, fan-out dispatch, and per-handler/per-bot rate limits.
- SQLite persistence with WAL mode, cursor recovery, and `export`/`import` via `pacto-bot-admin`.
- Structured diagnostics, metrics, last-run reports, and a schema-first contract in `schemas/`.
- Docker-free integration tests using in-process mock relay and bunker implementations.
- Generated Python SDK in `python/` with typed models, `PactoClient`, and a decorator-based `Bot` API.

Phase 2 and beyond (MLS group participation, on-chain governance reads/writes, webhook delivery) are planned but not yet implemented.

See [`docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`](docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md) for the full implementation plan and roadmap.

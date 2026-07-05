---
title: "Security overview and promises"
description: "What pacto-bot-api protects, what it assumes, and what guarantees it provides."
date: 2026-06-28
---

# Security overview and promises

`pacto-bot-api` is a single daemon that multiplexes one or more Pacto bot
identities and exposes a JSON-RPC API to bot handlers. This page explains the
security model in plain language.

## What the daemon is protecting

The daemon's most important job is to keep bot identities and conversations
under the operator's control. It does that by:

- owning the Nostr relay connections,
- owning the signing keys (or the connection to a bunker that holds them),
- decrypting incoming direct messages before forwarding them to handlers,
- making sure handlers can only act within the capabilities they were granted.

## Who is trusted for what

| Role | What they control | What the daemon assumes |
|---|---|---|
| **Operator** | The config file, the data directory, signing backends, relay selection | The operator is trusted to keep config and data directory secure. |
| **Daemon process** | The shared backend, the API sockets, the database | The daemon is the security boundary for everything below it. |
| **Handler** | Bot logic connected over Unix socket or HTTP | A handler is trusted only within its registered bot identities and capabilities. |
| **External services** | Relays, bunkers, EVM nodes | The daemon verifies identities and uses encrypted connections where possible. |

## What we promise

### Config and data directory are locked down

- The daemon refuses to start unless `pacto-bot-api.toml` is readable only by the
  owner (`0o600` or stricter).
- The Unix domain socket is created with `0o600` permissions, so only the same
  operating-system user can connect.
- The data directory uses an exclusive lock file (`daemon.lock`). Two daemon
  instances cannot run against the same data directory at the same time,
  preventing database corruption and duplicate event processing.

### Secrets never leave the daemon in plain text

- `nsec`, bunker URIs, and the HTTP secret token are never logged.
- They are never included in error messages sent to handlers.
- The HTTP token is stored in a file with owner-only permissions and compared
  using constant-time comparison.
- See [Key and secret handling](./key-and-secret-security.md) for details.

### Handlers are authorized per call, not just at connection time

When a handler registers, it declares:

- which bot identities it serves,
- which event types it handles,
- which capabilities it needs (for example, `SendMessages` or `ManageProfile`).

The daemon checks these permissions on **every** mutating call:

- `agent.send_dm` — the handler must be registered for the target `bot_id` and
  have the `SendMessages` capability.
- `agent.set_profile` — the handler must be registered for the target `bot_id`
  and have the `ManageProfile` capability.
- `agent.error` — treated as a mutating operation and authorized the same way.

A handler registered only for bot A cannot send messages as bot B.

### Bunker identities are verified

For both `bunker_local` and `bunker_remote`, the daemon asks the bunker for its
public key at startup and compares it to the configured `npub`. If they do not
match, the daemon exits with an error. For `bunker_remote`, only `wss://` relay
URLs are allowed.

### Abuse is bounded

- A handler cannot send multi-gigabyte frames: the transport drops any line
  longer than 1 MB.
- A handler cannot hold an unlimited number of connections open: the daemon
  accepts only a configured maximum per transport.
- A handler cannot flood mutating calls: a token-bucket rate limiter enforces
  10 mutating operations per second per handler, with a burst of 20, plus a
  per-bot aggregate limit.
- Idle connections are closed after a timeout.

### State survives a clean shutdown

On `SIGTERM` or `SIGINT` the daemon:

- persists event cursors to SQLite,
- notifies handlers that it is shutting down,
- closes relay and bunker connections,
- releases the lock file,
- flushes a last-run report.

The SQLite database uses WAL (write-ahead logging) mode, which reduces the
chance of corruption if the process crashes.

### Diagnostics are redacted

Health snapshots, error records, and the last-run report contain only public
identifiers such as `bot_id` and `npub`. Secrets are stripped before storage.
The `pacto-bot-admin diagnose --format json` output is safe to share for support
purposes as long as it does not include your config file.

## What we do not promise (Phase 1 limitations)

- **No cryptographic handler authentication beyond the transport.** On the Unix
  socket, any process running as the daemon's user can connect and act as any
  registered handler. On HTTP, anyone who knows the secret token can connect.
  Per-handler cryptographic credentials are planned for a later phase.
- **The `nsec` backend is not production-safe.** It is a development
  convenience. In production, use a bunker backend.
- **No protection against a fully compromised operating system.** If an attacker
  has root or can attach a debugger, they can read the daemon's memory. Use
  operating-system controls, encrypted swap, and least-privilege accounts.
- **HTTP is optional and less tightly bound than the Unix socket.** It is
  disabled by default.
- **Best-effort delivery.** The daemon does not guarantee that a handler will
  receive an event if it is disconnected when the event arrives.

## How to verify these claims

The repository includes tests that exercise these protections:

- `tests/secret_redaction.rs` — synthetic secrets are scanned for in memory,
  logs, error responses, and binary strings.
- `tests/transport_http.rs` and `tests/transport_unix.rs` — transport
  authentication and authorization.
- `tests/dispatch_integration.rs` — handler authorization and fan-out.
- `tests/daemon_startup.rs` — config permission checks, lock file behavior, and
  bunker pubkey verification.
- `tests/schema_sync.rs` — machine-readable schemas stay in sync with the code.
- `tests/requirement_coverage.rs` — every requirement has a covering test.

You can run them with `make test` (Docker-free, in-process mocks) or
`cargo test -- --ignored` when the `pacto-dev-env` Docker services are running.

## If something goes wrong

- Rotate the HTTP token with `pacto-bot-admin rotate-http-token`.
- Regenerate keys at the bunker side if a bunker URI may have been exposed.
- Stop the daemon before importing or exporting state; the admin CLI will refuse
  otherwise.
- Check `pacto-bot-admin diagnose --format json` for a safe, structured view of
  runtime health.

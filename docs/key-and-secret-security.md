---
title: "Key and secret handling in pacto-bot-api"
description: "How the daemon stores, uses, and protects signing keys and other secrets."
date: 2026-06-28
---

# Key and secret handling

The `pacto-bot-api` daemon sits between your bot's signing keys and the
handlers that implement bot behavior. This document explains how the daemon
protects those keys and what you can do to keep them safe.

## What secrets does the daemon touch?

Three kinds of sensitive material move through the daemon:

| Secret | What it is | Where it lives |
|---|---|---|
| `nsec` | The raw private key for a Nostr identity | Only in the optional `nsec` signing backend |
| Bunker URI | The address of a NIP-46 signing bunker | `bunker_local` or `bunker_remote` backend config |
| HTTP secret token | A password that protects the optional HTTP transport | `$DATA_DIR/bot_secret_token` |

The daemon **never logs** any of these values, and it **never returns** them in
error messages.

## Signing backends: three trust levels

The daemon supports three ways to sign events. They are listed from least to
most secure.

### 1. `nsec` — local test key (development only)

With this backend the config file contains the bot's raw `nsec`. The daemon
loads the key into memory, uses it to sign events, and clears the bytes from
memory when the signer is dropped.

What "clears from memory" means in practice:

- The key bytes are kept in a locked-down heap buffer wrapped by the `zeroize`
  crate. When the signer is destroyed, that buffer is overwritten with zeros
  before it is freed.
- While the public key is being derived, a temporary copy of the bytes exists on
  the stack. That temporary copy is also overwritten with zeros immediately
  after use.
- The daemon avoids the standard `SecretKey::parse` path because that path
  leaves extra unzeroed copies of the secret on the stack.

Even with these measures, the `nsec` backend is intended for **development and
testing only**. It is the least safe option because the secret exists in the
daemon's memory at all.

### 2. `bunker_local` — a bunker on the same machine

The daemon connects to a NIP-46 bunker running on the same host. The daemon
never sees the private key; it only asks the bunker to sign events. The bunker
URI is stored as a `SecretString` and is never logged.

When the daemon starts, it asks the bunker for its public key and compares it
to the `npub` in the config. If they do not match, the daemon refuses to start.
This prevents a misconfigured URI from causing the daemon to sign events as the
wrong identity.

### 3. `bunker_remote` — a production bunker over the internet

This is the recommended backend for production. The daemon connects to a remote
bunker over `wss://` (encrypted WebSocket). Plain `ws://` is rejected, so the
bunker URI cannot accidentally be used over an unencrypted relay connection.

The same pubkey verification happens at startup, and the private key never
leaves the bunker.

## The HTTP secret token

The optional localhost HTTP transport is protected by a randomly generated
token:

- Generated on first run using a cryptographically secure random number
  generator (256 bits, written as 64 hex characters).
- Stored in `$DATA_DIR/bot_secret_token` with file permissions `0o600` (only the
  owner can read or write it).
- Required in the `X-Pacto-Bot-Secret` header on every HTTP request.
- Compared using constant-time comparison to prevent timing attacks.
- Never written to logs, traces, or error responses.
- Can be rotated with `pacto-bot-admin rotate-http-token`; the daemon reloads it
  on `SIGHUP` or restart.

The HTTP transport is **disabled by default**. If you do not need it, leave it
off.

## How we test that secrets do not leak

The test suite includes dedicated secret-redaction tests. For each run it
creates unique synthetic secrets and checks that those secrets do not appear in:

- log output,
- JSON-RPC error responses,
- the release binary's strings,
- a simulated core dump read from `/proc/self/mem` (on Linux).

These tests run in CI on every pull request.

## What we cannot promise

`zeroize` and careful handling reduce the risk of secrets surviving in memory,
but they cannot remove every risk. In particular:

- **Swap and hibernation**: if the operating system swaps the daemon's memory to
  disk, or hibernates the machine, the secret may be written to persistent
  storage. Run the daemon on machines with encrypted swap, or disable swap, if
  this matters for your threat model.
- **Kernel and root access**: a process running with root privileges, or one
  that can attach a debugger to the daemon, can read its memory. These are
  operating-system-level risks, not bugs in the daemon.
- **Environment variables**: if you set the `nsec` via an environment variable,
  that variable is visible to other processes on the same machine (for example,
  via `/proc/<pid>/environ`). Prefer the bunker backends in production.
- **TOML parser copies**: the config file is parsed before the secret is moved
  into the protected signer. Use strict file permissions and avoid checking
  real secrets into version control.

## Recommendations

- In production, always use `bunker_remote`.
- Keep config files at `0o600` or stricter; the daemon refuses to start
  otherwise.
- Disable HTTP unless you need it, and guard the token file carefully.
- Rotate the HTTP token periodically and after any suspected exposure.
- Do not run the daemon as root.

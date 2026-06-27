# Security & Architecture Review: pacto-bot-api Daemon Plan

> **Date:** 2026-06-25  
> **Plan reviewed:** `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`  
> **Reviewers:** 3 parallel agents — Key Management Security, Transport & API Security, Identity & Lifecycle Architecture  
> **Status:** All 15 findings resolved in plan v2

---

## Summary

| Severity | Count | Resolved |
|----------|-------|----------|
| Critical | 3 | 3 |
| High | 6 | 6 |
| Medium | 6 | 6 |
| **Total** | **15** | **15** |

---

## Critical

### C1. No bunker pubkey verification at daemon connection time

**Reviewer:** Key Management Security  
**Affected:** U2 (signer.rs, bunker.rs), U4 (nostr.rs), KTD-3

**Description:** The daemon connects to a NIP-46 bunker via `BunkerConnection::connect(bunker_uri)` and uses it to sign events, but the plan never specifies that the daemon verifies the bunker's pubkey matches the configured `npub` for that bot. The `test-bunker` admin command (U12) does this verification as a one-shot check, but the daemon's own startup path (U2, U9) does not.

This means:
- A misconfigured `bunker_uri` pointing to a different bunker would cause the daemon to sign events with the wrong identity — the daemon would publish gift wraps signed by a key that doesn't match the bot's npub, breaking DM decryption for recipients and potentially allowing impersonation if the wrong bunker is controlled by an attacker.
- A MITM on the bunker relay connection (especially `ws://` for `bunker_local`) could substitute a different bunker and the daemon would sign with whatever key the attacker's bunker provides.

**Resolution:** In `BunkerConnection::connect` (or immediately after), call `get_public_key()` on the bunker and assert that the returned pubkey matches the bot's configured `npub` from `BotConfig`. Fail the connection with a clear error if they don't match. This is a hard startup failure, not a warning — signing with the wrong identity is worse than not signing at all. Additionally, for `bunker_remote` backends, require `wss://` (not `ws://`) in the bunker URI to prevent plaintext relay connections in production.

**Applied to:** R6, U2, U9

---

### C2. No split-brain prevention

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R5, R8, R10, R21, U3, U9, KTD-2

**Description:** Two daemon instances started with the same config (or same `data_dir`) will both connect to the same bunker, subscribe to the same relays, receive duplicate events, compete for signing, and concurrently write to the same `agent.db` — causing duplicate messages on the network and potential database corruption.

**Resolution:** Add a file-lock mechanism at daemon startup: acquire an exclusive flock on `$DATA_DIR/daemon.lock` before opening `agent.db`. Exit immediately with a clear error if the lock is held. Document that each daemon instance needs its own `data_dir`.

**Applied to:** R21, U9

---

### C3. `handler_id` is client-provided in API spec but server-assigned in U6

**Reviewer:** Transport & API Security  
**Affected:** API spec table, U6, R15

**Description:** `handler_id` is listed as a client-provided parameter in the API spec but described as server-assigned in U6. If client-provided, a malicious handler can register with another handler's ID to intercept events, or call `handler.unregister` with any handler's ID to disrupt them. The contradiction must be resolved in favor of server-assigned IDs.

**Resolution:** Remove `handler_id` from `handler.register` request params. Daemon generates a UUIDv4 on successful registration and returns it. All subsequent messages referencing a handler (`unregister`, `agent.error`) must validate against the connection the message arrived on, not a client-supplied string.

**Applied to:** R15, U6, API spec

---

## High

### H1. `agent.send_dm` lacks per-message bot identity authorization

**Reviewer:** Key Management Security + Transport & API Security (found independently)  
**Affected:** R14, U7 (dispatch.rs), U6 (handlers.rs)

**Description:** A handler registers for bot A (`echo-bot`) but can send `agent.send_dm {bot_id: "treasury-bot", ...}` and the daemon will sign and publish the DM as `treasury-bot`. The handler registration (U6) validates that the handler's declared `bot_ids` exist in config at registration time, but the `agent.send_dm` notification handler (U7 dispatch) takes a free-form `bot_id` parameter and there is no mention of checking that the sending handler's connection is authorized to act as that bot. The capability check at registration is only enforced at `handler.register` time, not per-message. A compromised or buggy handler for a low-privilege bot can send DMs as a high-privilege bot.

**Resolution:** In the `agent.send_dm` handler (dispatch.rs), look up the handler's connection from the `HandlerRegistry` and verify that `bot_id` from the request is in the handler's registered `bot_ids` list. Reject with error code `-32006` (Unauthorized bot_id) if the handler is not authorized for that bot. This check also applies to `agent.set_profile` and any future handler→daemon notifications that take a `bot_id` parameter. Add a `HandlerRef::is_authorized_for(&self, bot_id: &str) -> bool` method to centralize this check.

**Applied to:** R14, R16, U7

---

### H2. nsec held in memory as plain `String` with no zeroization

**Reviewer:** Key Management Security  
**Affected:** R6, R7, KTD-3, U2 (signer.rs, config.rs)

**Description:** The `nsec` backend (KTD-3) stores the raw nsec hex in a `SigningConfig { nsec: Option<String> }` field, which is then parsed into `nostr_sdk::Keys` and held in `SignerBackend::LocalKey { keys: Keys }`. The plan mentions no use of `zeroize`, `secrecy::Secret`, `memguard`, or any memory protection.

This means:
- The nsec bytes persist in memory after the `String` is parsed — Rust's `String` does not zero on drop.
- Core dumps, debugger attachment, or `/proc/<pid>/mem` would expose the raw nsec.
- The `${ENV_VAR}` expansion path reads from environment variables, which are visible in `/proc/<pid>/environ` and may be logged by process monitors or orchestration tools.

While the plan labels this "dev mode" and logs a warning, there is no technical enforcement preventing production use — a warning in logs is not a security control.

**Resolution:** The `LocalKey` variant wraps keys in a `Zeroizing` wrapper (`zeroize` crate) so the nsec bytes are cleared from memory on drop. Add a test that verifies zeroization with `zeroize`'s `assert_is_zeroized`. Document that the `nsec` backend is for development only and the warning log is a courtesy, not a security boundary.

**Applied to:** R6, KTD-3, U2

---

### H3. `bot_id` uniqueness never validated

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R8, U1, U8

**Description:** `bot_id` uniqueness is never validated. R8 defines `bot_id` as a daemon-local label, but U1's config validation tests check for missing `npub`/`bunker_uri` — not duplicate `bot_id`. The `cursors` table uses `bot_id TEXT PRIMARY KEY`, so duplicate `bot_id` values cause cursor persistence failures. The `agent.event` notification uses `bot_id` as a parameter, so handlers cannot distinguish between two bots with the same label.

**Resolution:** Add a uniqueness check in `DaemonConfig::load()`: collect all `bot_id` values, detect duplicates, return a validation error. Add test scenario: "Config with duplicate `bot_id` returns a validation error."

**Applied to:** R7, U1, U9

---

### H4. Admin CLI can corrupt `agent.db` while daemon is running

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R10, R19, U8, U12, KTD-8

**Description:** U12's `import` subcommand inserts rows into `agent.db` directly, sharing the `db` module with the daemon. If the daemon is concurrently writing cursors (U8: "after each successfully processed event"), two processes write to the same SQLite file — `SQLITE_BUSY` at best, WAL corruption at worst. The `export` subcommand risks reading partially-written state.

**Resolution:** Require the daemon to be stopped before `import`/`export`. The admin CLI checks for the lock file (from C2's mitigation) and refuses to operate if the daemon is running.

**Applied to:** R10, U12

---

### H5. HTTP transport has no authentication in Phase 1

**Reviewer:** Transport & API Security  
**Affected:** R2, U5, OQ3

**Description:** The Unix socket has `0o600` kernel-enforced access control. The HTTP transport on `127.0.0.1:9800` has no equivalent — any process on the machine can connect, register as a handler, receive events, and send DMs as any bot. OQ3 acknowledges this gap but defers `secret_token` to Phase 2, leaving Phase 1 with an unauthenticated HTTP transport alongside the authenticated Unix socket.

**Resolution:** Require a `secret_token` for the HTTP transport in Phase 1. Generate on first run, store with `0o600` permissions, validate as a header on every HTTP request. Make HTTP opt-in behind `--enable-http` (off by default) with a startup warning when used without a token.

**Applied to:** R2, U5

---

### H6. Capabilities validated at registration, not per-call

**Reviewer:** Transport & API Security  
**Affected:** R5, R7, U6, U7, API spec

**Description:** Capabilities are validated at registration time (handler's declared capabilities must be a subset of bot's configured capabilities), but there is no per-call enforcement. A handler registered with `ReadMessages` can still call `agent.send_dm` and `agent.set_profile`. The capability check gates what the handler *declares*, not what it *does*.

**Resolution:** Add per-call capability checks: before processing `agent.send_dm`, verify the handler's registration includes `SendMessages` for the target `bot_id`. Before `agent.set_profile`, verify `ManageProfile`. Map each mutating API method to a required capability and enforce on every invocation.

**Applied to:** R16, U7

---

## Medium

### M1. WAL mode mentioned in risk table but not in U8 implementation

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R19, R20, R22, U8

**Description:** WAL mode is specified in the risk table ("WAL mode. Periodic cursor flushes") but U8's implementation description and test scenarios never mention `PRAGMA journal_mode=WAL`, `PRAGMA synchronous=NORMAL`, or any durability configuration. Without WAL, a crash during a cursor write can leave the database inconsistent.

**Resolution:** Explicitly add to U8: `Database::open` must execute `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL`. Add test scenario: "Database survives process kill without corruption."

**Applied to:** R19, U8

---

### M2. Config drift at runtime

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R7, R8, R11, R21, U8, U9

**Description:** The daemon loads config once at startup (Phase 1 defers runtime bot add/remove to Phase 3). If an operator changes a bot's npub but keeps the same `bot_id`, then restarts, the old cursors (keyed by `bot_id` only) are applied to the new identity — replaying events from the wrong bot's history. The `cursors` table has no `npub` column to detect this mismatch.

**Resolution:** Add an `npub` column to the `cursors` table. On startup, validate that the stored npub matches the config's npub for that `bot_id`. If they differ, log a warning and reset the cursor. Add a `pacto-bot-admin validate-config` subcommand that cross-references config with `agent.db` state.

**Applied to:** R19, U8, U12

---

### M3. `bot_id` to npub mapping is implicit and underspecified

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R8, R16, U3, U7

**Description:** `agent.event` and `agent.status` notifications use `bot_id`, `handler.register` accepts `bot_ids`, but `ClientManager` keys on `PublicKey` (npub). The dispatch layer needs a `bot_id` → npub mapping to route events, but the plan never specifies where this mapping lives or how it's built.

**Resolution:** Add a `bot_id_map: HashMap<String, PublicKey>` to `ClientManager`. Populate it from config at startup. Add `get_bot_by_id(&self, bot_id: &str) -> Option<&BotState>` method. Document the bidirectional mapping.

**Applied to:** R5, U3

---

### M4. No maximum JSON-RPC frame size

**Reviewer:** Transport & API Security  
**Affected:** R3, U5, KTD-4

**Description:** JSON-RPC frames are newline-delimited with no length prefix and no maximum size. A malicious handler can send a multi-gigabyte frame, causing unbounded memory allocation and OOM crash (DoS). The risk table addresses connection storms but not per-message size.

**Resolution:** Enforce a maximum frame size of 1 MB in the transport read loop. If a frame exceeds the limit before encountering newline, close the connection and log a warning. Make the limit configurable.

**Applied to:** R3, U5

---

### M5. No rate limiting in Phase 1

**Reviewer:** Transport & API Security  
**Affected:** R14, U7, Phase 3 deferral

**Description:** Rate limiting is deferred to Phase 3. A handler can flood `agent.send_dm` at line speed, consuming bunker signing capacity, relay bandwidth, and risking the bot being banned from relays for spam. No daemon-side enforcement exists in the Phase 1 plan.

**Resolution:** Add a simple token-bucket rate limiter per handler connection for mutating operations (`agent.send_dm`, `agent.set_profile`). Default 10/sec with burst 20. Reject over-limit calls with error code `-32005` (Rate limited). This is a small addition that prevents the most obvious abuse without requiring full Phase 3 infrastructure.

**Applied to:** R18, U7

---

### M6. Handler re-registration on restart is ambiguous

**Reviewer:** Identity & Lifecycle Architecture  
**Affected:** R20, U8

**Description:** R20 says handlers are "re-registered" on restart but connections are dead — the plan is ambiguous about what re-registration means. Does the daemon hold dead connections open? Does it remember bindings and verify them when handlers reconnect?

**Resolution:** Clarify R20: re-registration means the daemon remembers handler bindings (bot_ids, event_types, capabilities) and verifies them when handlers reconnect; it does not hold dead connections open. The persisted data is for verification on reconnect, not for maintaining zombie connections.

**Applied to:** R20

---

## Error Codes Added

| Code | Meaning | Added for |
|------|---------|-----------|
| `-32005` | Rate limited — the handler exceeded the per-connection rate limit | M5 |
| `-32006` | Unauthorized bot_id — the handler is not authorized to act as the specified bot | H1 |

---

## Overlapping Findings

H1 was found independently by both the Key Management Security and Transport & API Security reviewers — the same `agent.send_dm` authorization gap seen from two angles (key management and API security). This is the highest-confidence finding in the review.

C3 and H6 are related: both stem from the handler registration model being underspecified about what a handler identity *is* and what it authorizes.

---

*Review conducted 2026-06-25. All findings resolved in plan v2 at `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`.*

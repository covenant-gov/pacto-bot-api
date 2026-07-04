---
date: 2026-07-01
topic: nostrbotkit-adoption
---

# NostrBotKit Feature Adoption for `pacto-bot-api`

## Summary

Adopt optional, non-core convenience layers from NostrBotKit (`nbk` v0.5.2) that broaden `pacto-bot-api` from a developer/runtime daemon into a tool that operators and non-programmers can also use. The JSON-RPC handler model remains the primary extension point; new features are either HTTP surfaces around that model or internal "simple handler" components that call the same publish paths as external handlers.

---

## Problem Frame

`pacto-bot-api` already multiplexes Nostr identities, relay connections, and signing, and exposes a language-agnostic JSON-RPC API. That design is correct for developers who want to write bot logic in any language. However, NostrBotKit proves that several operator-facing conveniences move adoption:

- External systems need to trigger bots over HTTP (webhooks).
- Operators want to forward events and actions to audit logs or automation (outbound callbacks).
- Time-based bots need cron without keeping a handler process alive or writing polling loops.
- Non-programmers want YAML-configurable bots for common patterns (weather, RSS, Q&A).
- Paid/premium bot features need Lightning/NWC integration.
- Group-chat bots need Marmot/MLS support.

The goal is not to turn `pacto-bot-api` into NostrBotKit. It is to add optional layers that broaden the user base while preserving the daemon/CLI/handler separation, the schema-first contract, and the security posture established in the architecture plan.

---

## Requirements

### Feature Assessment

| Feature | Effort | Priority | Notes |
|---------|--------|----------|-------|
| Inbound webhooks | Low-Medium | High | Reuses existing HTTP transport and dispatch; Phase 3 precursor. |
| Outbound `notify_url` callbacks | Low | High | Tiny change, huge integration value. |
| Cron / scheduled jobs | Medium | Medium | Common bot need; requires scheduler persistence. |
| Declarative command catalog | Medium-High | Medium | Broadens user base; ship incrementally (reply/api_call first). |
| Lightning / NWC integration | Medium-High | Medium-High | Ecosystem fit; defer until Phase 2 planning stabilizes. |
| Marmot / MLS group chat | High | High (Phase 2) | Dependency- and design-gated. |
| Web admin UI (read-only) | Medium | Medium | Operator convenience; keep write operations in CLI. |
| Blueprint / template system | Medium | Medium | CLI-only; no daemon changes; pairs with declarative catalog. |
| Media uploads (NIP-96/Blossom) | Medium | Medium | Pair with declarative `post` command type. |
| DM-first runtime control | — | Non-goal | Conflicts with KTD-8; do not adopt. |

### Inbound HTTP Webhooks

- R38. Add `POST /webhook/:bot_id/:trigger` to the existing localhost HTTP transport. The route is authenticated by a bearer token and only accepts requests for bot identities configured in `pacto-bot-api.toml`. (References R2, R5, R7, KTD-9.)
- R39. Webhook secrets are generated with a CSPRNG, stored at `0o600` or stricter, and never appear in logs, error responses, or diagnostics. Rotation is performed via `pacto-bot-admin rotate-webhook-secret`. (References R34, KTD-10, KTD-11.)
- R40. A webhook hit creates a synthetic `webhook_received` event and dispatches it as an `agent.webhook` notification to handlers registered for the target `bot_id` and `event_type`. Handlers reply with the normal `handler.response` actions (`ack`, `reply`, `defer`, `ignore`). (References R15, R16, R18.)
- R41. The daemon returns the first `reply` content as `text/plain` within a configurable timeout (default 5 seconds). If no handler replies or the timeout elapses, the daemon returns `202 Accepted`. (References R30.)
- R42. Webhooks are rate-limited per `bot_id`/`trigger` path to prevent abuse.

### Outbound `notify_url` Callbacks

- R43. Extend `handler.register` to accept optional `notify_url` and `notify_headers`. The daemon persists these with the handler registration. (References R15, R19, R20.)
- R44. After delivering an `agent.event` to a handler with a configured `notify_url`, the daemon POSTs a metadata copy of the event to that URL. The payload excludes decrypted message content to avoid leaking plaintext. (References R14, R34.)
- R45. After a successful `agent.send_dm` or `agent.set_profile` call from an authorized handler, the daemon POSTs a callback with event id, `bot_id`, method, and recipient metadata only. (References R14.)
- R46. Callbacks are delivered via a bounded channel and background task. A slow or failing `notify_url` must not block event dispatch, cursor advancement, or relay publish. (References R27, R30.)
- R47. Secrets are redacted from `notify_headers` and payloads; header values are treated with the same hygiene as the HTTP token. (References R34, KTD-11.)

### Cron / Scheduled Jobs

- R48. Operators can define per-bot cron jobs in `pacto-bot-api.toml` with a name, cron expression, timezone, and either an `event_type = "cron_fired"` payload or a built-in action. (References R7, R25.)
- R49. The daemon persists cron jobs in SQLite with `last_run_at`, `next_run_at`, and a JSON payload so jobs survive restarts and missed runs can be caught up. (References R19, R20.)
- R50. At the scheduled time, the daemon dispatches a `cron_fired` event as `agent.event` to registered handlers. Built-in actions (e.g., sending a static DM) are explicitly marked and executed without a connected handler. (References R15, R18.)
- R51. A scheduler task wakes at the next minute boundary, evaluates due jobs in the configured timezone, and updates run times after execution. (References R30.)

### Declarative Command Catalog

- R52. Add an internal "simple handler" that reads a per-bot command catalog from `commands/{bot_id}.toml` or an inline `[[bots.commands]]` block and executes configured commands by calling the same internal publish paths as JSON-RPC handlers. (References KTD-8.)
- R53. The catalog supports command types incrementally: `reply` (static/templated text), `api_call` (HTTP GET/POST with JSON parsing and template rendering), `post` (publish a Kind 1 note), `rss_feed` (poll feed and post new items), and `composite` (parallel HTTP calls). (References R32.)
- R54. The simple handler registers for `dm_received` and `cron_fired` events and matches messages by trigger prefix. It is subject to the same capability and rate-limit checks as external handlers. (References R14, R16, R18.)
- R55. Dialog/menu sessions, if supported, are scoped per-sender, stored in SQLite, and time out after a configurable period. (References R19, R20.)
- R56. `pacto-bot-admin` provides `add-command` and `remove-command` to edit catalog files safely, refusing to run while the daemon lock is held. (References R9, R21, KTD-8.)

### Lightning / NWC Integration

- R57. Add an optional per-bot `nwc` backend configured via an environment variable or a secrets file with `0o600` permissions. The daemon initializes an NWC client only when the URI is present. (References R6, R7, R34, KTD-11.)
- R58. Expose JSON-RPC methods `agent.make_invoice`, `agent.pay_invoice`, `agent.lookup_invoice`, and `agent.get_balance`, authorized by the handler's registration capabilities. (References R14, R16, R32.)
- R59. Forward NWC notifications (`payment_received`, `payment_sent`) as `agent.payment` events and kind 9735 zap receipts as `agent.zap_received` events to registered handlers. (References R13, R15.)
- R60. `pay_invoice` and `make_invoice` calls are rate-limited and logged to prevent wallet draining or abuse. (References R18, R34.)
- R61. Support the `lud16` field in `agent.set_profile` and profile publishing. (References R32.)

### Marmot / MLS Group Chat (Phase 2)

- R62. Add `marmot = true` and `marmot_relays` to `BotConfig`. For each bot with Marmot enabled, spawn a task that owns an `MDK<MdkSqliteStorage>` instance and subscribes to relevant key-package and group-message kinds. (References R5, R7.)
- R63. Decrypt incoming Marmot messages and dispatch `agent.marmot_message` events with `group_name`, `sender`, `content`, and `event_id`. Add `agent.send_marmot_message` for handlers to reply. (References R14, R15, R32.)
- R64. Store Marmot state in a per-bot SQLite database under `$DATA_DIR/marmot/{bot_id}/state.db`. The DB encryption key is derived from the signing key for `nsec` backends and from a separate config secret for bunker backends. (References R19, R20, KTD-11.)

### Web Admin UI

- R65. Add a read-only daemon dashboard at `GET /admin` served by the existing HTTP transport, authenticated by `X-Pacto-Bot-Secret` or a separate admin secret. It may display daemon status, registered handlers, metrics, and diagnostics. (References R2, R31, R35, KTD-9.)
- R66. The admin UI does not create, delete, or edit bot identities, signing config, or secrets. Write operations remain in `pacto-bot-admin`. (References R9, KTD-8.)

### Blueprint / Template System

- R67. Add `pacto-bot-admin new --template <name>` to load a parameterized bot-identity blueprint, prompt for parameters, and append the resolved config snippet to `pacto-bot-api.toml`. (References R9, KTD-8.)
- R68. Blueprints live in `templates/bots/` and must validate against `schemas/config.json` before the resolved config is written. (References R32.)

### Media Uploads

- R69. Add `agent.upload_media` to upload files to a NIP-96 or Blossom server with NIP-98 auth, returning a public URL for handlers to include in `agent.send_dm` or `agent.post`. (References R32.)
- R70. File uploads over the localhost HTTP transport are size-limited and validated to prevent abuse. (References R3, R34.)

### Explicit Non-Goals

- R71. The daemon never creates or deletes bot identities via DMs or any runtime mechanism. Bot lifecycle stays in `pacto-bot-admin`. (References R9, R11, KTD-8.)

---

## Key Technical Decisions

- **KTD-NB-1. Inbound webhooks dispatch as `agent.webhook` events, not built-in commands.** The daemon converts an HTTP hit into a synthetic event and lets handlers decide the response. This preserves the daemon/handler separation and avoids turning the daemon into a command interpreter. A future declarative command catalog can receive the same events.
- **KTD-NB-2. Outbound callbacks use a bounded channel and background task.** Callbacks must not block relay publish, cursor advancement, or event dispatch. A bounded channel with drop/backpressure logic keeps the daemon resilient against slow endpoints.
- **KTD-NB-3. Cron jobs are event-driven rather than command-driven.** At the scheduled time the daemon dispatches a synthetic `cron_fired` event to registered handlers. Built-in actions are an explicit subset, not the default, so the daemon does not become a command interpreter.
- **KTD-NB-4. Declarative command catalog is an internal simple handler.** It reads a catalog and executes actions by calling the same internal publish functions used by `agent.send_dm` and `agent.set_profile`. This preserves JSON-RPC as the primary extension point and lets non-programmers use YAML without forcing programmers to abandon custom handlers.
- **KTD-NB-5. NWC is an optional per-bot backend, not a global daemon feature.** Only bots configured with an NWC URI participate in payments. This matches the static multi-bot config model and avoids loading payment logic for bots that do not need it.
- **KTD-NB-6. Web admin UI is read-only in the daemon.** Lifecycle write operations stay in `pacto-bot-admin`. A richer local admin UI belongs in the CLI, not the daemon, to preserve KTD-8.
- **KTD-NB-7. Blueprint templates live in the CLI.** Identity creation is a lifecycle/admin operation, so templating belongs in `pacto-bot-admin`, not the daemon.
- **KTD-NB-8. All new HTTP surfaces remain loopback-only.** Inbound webhooks and the admin UI are served on the same localhost HTTP transport as the existing JSON-RPC endpoint. Production exposure must be through a reverse proxy, not by widening the bind address.

---

## Acceptance Examples

- AE1. Inbound webhook triggers a handler reply
  - **Given:** A bot `echo-bot` is configured with a webhook secret. A handler is registered for `echo-bot` and event type `webhook_received` with `reply` capability.
  - **When:** An external system POSTs to `/webhook/echo-bot/alert` with the bearer token and body `server down`.
  - **Then:** The handler receives an `agent.webhook` notification with `trigger=alert` and `body=server down`; it replies with `acknowledged`; the daemon returns `acknowledged` as `text/plain` within 5 seconds.

- AE2. Outbound `notify_url` fires on a sent DM
  - **Given:** A handler registers for `echo-bot` with `notify_url = http://localhost:9000/events` and `SendMessages` capability.
  - **When:** The handler sends `agent.send_dm` for `echo-bot` and the daemon successfully publishes the gift wrap.
  - **Then:** Within 1 second the daemon POSTs to `http://localhost:9000/events` a JSON payload containing `bot_id`, `method: agent.send_dm`, `recipient`, and `event_id`, but no decrypted content.

- AE3. Cron job dispatches a scheduled event
  - **Given:** A bot `digest-bot` has a cron job named `morning` with schedule `0 9 * * *` and `event_type = "cron_fired"`. A handler is registered for `digest-bot` and `cron_fired`.
  - **When:** The local time reaches 09:00 in the configured timezone.
  - **Then:** The daemon dispatches an `agent.event` with `type: cron_fired` and `payload` from the config; the scheduler updates `last_run_at` and `next_run_at` in `agent.db`.

- AE4. Declarative command replies without a custom handler
  - **Given:** `commands/weather-bot.toml` defines a `/weather` command of type `api_call` with URL `https://wttr.in/{args}?format=j1` and template `{{ current_condition.0.temp_C }}°C in {{ nearest_area.0.areaName.0.value }}`. The simple handler is enabled for `weather-bot` with `SendMessages`.
  - **When:** A user sends the DM `/weather berlin` to `weather-bot`.
  - **Then:** The daemon fetches the URL, renders the template, and sends a DM reply `22°C in Berlin` without any external handler process running.

- AE5. NWC invoice creation flows through JSON-RPC
  - **Given:** A bot `premium-bot` is configured with a valid NWC URI and a handler registered for `premium-bot` with the `ManagePayments` capability.
  - **When:** The handler calls `agent.make_invoice` with `amount_sats: 100` and `description: report`.
  - **Then:** The daemon returns a BOLT11 invoice string and fires a `notify_url` callback (if configured) with `method: agent.make_invoice` and the invoice metadata.

---

## Scope Boundaries

### In Scope

- Inbound webhooks with bearer-token auth and handler event dispatch.
- Outbound `notify_url` callbacks on event delivery and mutating actions.
- Persisted cron jobs that dispatch synthetic events or execute built-in actions.
- Declarative command catalog for common patterns (reply, api_call, post, rss_feed, composite), shipped incrementally.
- Lightning/NWC integration with invoice creation, payment, lookup, balance, and zap notifications.
- Read-only web admin UI for daemon status and diagnostics.
- Blueprint/template system for `pacto-bot-admin new`.
- Media uploads via NIP-96/Blossom with NIP-98 auth.

### Deferred to Later Phases

- **Marmot/MLS group chat** (Phase 2): gated by `mdk-core`/`mdk-sqlite-storage` availability and the DB encryption key design for bunker-backed bots.
- **Full dialog/menu sessions** in the declarative catalog: ship reply/api_call/post first; stateful multi-turn dialogs are a follow-up.
- **Payment-gated commands** in the declarative catalog: useful once NWC and the catalog are both stable.
- **Stateful webhook redelivery** with persistent offsets: webhooks are synchronous best-effort in the first slice.
- **Hot reload of config** for new commands or cron jobs: requires SIGHUP/config-watching deferred to Phase 3.

### Out of Scope

- **DM-first runtime control (AlphaBot):** NostrBotKit allows creating, starting, stopping, and deleting bots via DM. This directly conflicts with KTD-8 and R9/R11. Reject this feature.
- **Daemon identity creation/deletion:** The daemon never creates or deletes bot identities.
- **Public-facing HTTP:** All HTTP surfaces remain loopback-only; reverse proxies handle external exposure.
- **Native client SDKs** for specific languages: these are Phase 4 per the architecture plan.

---

## First Slice Recommendation

Deliver the highest-value, lowest-risk pair: **inbound webhooks** plus **outbound `notify_url` callbacks**. This slice proves both inbound and outbound HTTP integration patterns without touching NWC, cron, or the declarative catalog.

### Files to touch

- `schemas/jsonrpc.json` — add `webhook_received` to `handler.register` event types; add `notify_url` and `notify_headers` to `handler.register` params; add `agent.webhook` notification shape.
- `src/transport/http.rs` — add `POST /webhook/:bot_id/:trigger` route, bearer-token verification, and `webhook_event` construction.
- `src/handlers.rs` — extend `HandlerRef` with `notify_url` and `notify_headers`.
- `src/db.rs` — add `notify_url` and `notify_headers` columns to `handler_registrations`; add `webhook_secrets` table or reuse the existing secret storage pattern.
- `src/dispatch.rs` — dispatch `webhook_received` events; fire callbacks after event delivery and after successful `agent.send_dm`/`agent.set_profile`.
- `src/config.rs` — add `[http] webhook_enabled` and `webhook_secret` parsing (env/file reference, `0o600`).
- `src/admin.rs` — add `rotate-webhook-secret` and `test-webhook` subcommands.
- `Cargo.toml` — verify `subtle` is available for constant-time token comparison.

### Methods to add

- `agent.webhook` notification: `{bot_id, trigger, body, query_params, headers}`.
- `handler.register` gains optional `notify_url` and `notify_headers`.
- `pacto-bot-admin rotate-webhook-secret` and `pacto-bot-admin test-webhook --bot-id <id> --trigger <trigger>`.

### Tests to write

- `tests/transport_http.rs` — webhook hit returns handler reply; webhook with bad token returns `401 Unauthorized`; webhook with no handler reply returns `202`.
- `tests/dispatch_integration.rs` — `notify_url` callback fires after event delivery and after `agent.send_dm`; callback payload omits content; slow callback does not block cursor advancement.
- `tests/secret_redaction.rs` — webhook token and `notify_headers` do not appear in logs, error responses, or binary strings.
- `tests/admin_cli_webhook.rs` — `rotate-webhook-secret` rewrites the secret file with `0o600`; `test-webhook` sends a synthetic hit and reports the response.

### Why this slice

It is low-risk, high-value, and reuses existing HTTP transport, dispatch, and capability machinery. It also establishes the pattern for all later inbound/outbound HTTP features without changing the daemon/CLI separation or introducing large new subsystems.

---

## Risks & Open Questions

### Security Risks

- **Webhook tokens:** Bearer tokens must be compared in constant time (`subtle`), stored at `0o600`, and never logged. The localhost binding reduces but does not eliminate exposure if an attacker can execute code on the same machine.
- **Outbound callback metadata leaks:** `notify_url` payloads include `recipient` pubkeys and method names. Operators must be aware that these are metadata, not plaintext, but still sensitive. We must not include decrypted content or raw event IDs that could be used to reconstruct conversations.
- **NWC URI:** The NWC URI is a secret comparable to `nsec` and bunker URIs. It must be stored in env vars or files with `0o600`, redacted from logs and errors, and cleared from memory on drop where possible.
- **Template injection in declarative commands:** User-supplied URLs in `api_call` and `composite` commands are a server-side request forgery (SSRF) and template-injection risk. URLs must be validated against an allowlist, templates must be sandboxed, and network calls must be rate-limited.
- **Admin UI exposure:** A read-only UI still increases attack surface. It must use the existing secret token and remain loopback-only; operators will misconfigure it behind reverse proxies.

### Performance Risks

- **Synchronous webhook replies:** Waiting for handler replies can hang if a handler is slow. The timeout must be short and well-tested; a degraded handler must not block the HTTP response beyond the timeout.
- **Outbound callback backpressure:** A slow or down `notify_url` endpoint could fill a bounded channel. Decide drop-vs-block semantics and expose metrics for dropped callbacks.
- **Cron catch-up storms:** If the daemon is down across many scheduled minutes, waking up could fire many jobs at once. The scheduler should catch up one missed run per job, not all missed minutes.
- **NWC polling:** Payment polling every 10 seconds per invoice can become expensive. Batch or event-driven notification is preferable once relay subscriptions for NWC notifications are reliable.

### Dependency Risks

- **`mdk-core` / `mdk-sqlite-storage`:** Marmot support requires crates that are not yet on crates.io. This conflicts with the standalone-repo KTD-1. Do not add git/path dependencies for Phase 1; defer Marmot until the crates are published or a deliberate exception to KTD-1 is approved.
- **`cron` and `chrono-tz`:** New dependencies for cron parsing and timezone handling. They are standard and well-maintained, but add to the audit surface.
- **`minijinja` or `handlebars`:** Needed for runtime templating in the declarative catalog. Evaluate against the existing scaffold template engine before choosing.

### Open Questions

- Should `notify_url` callbacks fire for **every** event delivered to a handler, or only for mutating actions (`agent.send_dm`, `agent.set_profile`)? The research favors both; the first slice should pick one and document the rationale.
- Should the webhook endpoint wait for **all** matching handlers or return the **first** reply? The research recommends first reply to avoid coordination complexity; confirm this is acceptable.
- Should built-in cron actions (e.g., sending a static DM) be included in the first cron slice, or should cron be event-only until the declarative catalog exists?
- For bunker-backed bots, how should the Marmot DB encryption key be derived? Research flags this as a hard design problem; defer to Phase 2.
- Should the web admin UI use the same `X-Pacto-Bot-Secret` as the JSON-RPC HTTP transport, or a separate admin secret? Reusing the token is simpler; a separate secret reduces blast radius.

---

## Sources / Research

- `docs/research/nostrbotkit-features-deep-dive.md` — primary source for NostrBotKit feature mapping and implementation sketches.
- `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` — existing requirements R1–R37 and Key Technical Decisions KTD-1 through KTD-15.
- `docs/pacto-bot-admin-llms.txt` — operator model and CLI command context.
- `schemas/jsonrpc.json` — existing JSON-RPC catalog and method shapes.

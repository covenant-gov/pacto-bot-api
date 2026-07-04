# Deep Dive: NostrBotKit Features for `pacto-bot-api`

Research date: 2026-07-01  
Source project: [NostrBotKit](https://codeberg.org/Tuxor/NostrBotKit) (`nbk` v0.5.2)  
Local repository: `pacto-bot-api` (v0.4.1)

This document broadens the research started in `docs/research/nostrbotkit-comparison.md`. For each borrowable feature it explains:

1. What NostrBotKit actually does (with source references).
2. The relevant standards, crates, and protocols.
3. How the feature maps onto `pacto-bot-api`’s existing architecture.
4. A concrete implementation sketch.
5. What new abilities it would unlock for operators and developers.
6. Effort, risks, and a recommended priority.

---

## 1. Framing: why these features matter

`pacto-bot-api` is intentionally a **developer/runtime daemon**: it multiplexes bot identities, relay connections, and signing, and exposes a language-agnostic JSON-RPC surface. NostrBotKit is an **operator appliance**: one Rust binary where non-programmers configure bots via DM commands and YAML files. The two projects are not direct competitors, but NostrBotKit proves which operator-facing conveniences move the needle for adoption.

The goal is not to turn `pacto-bot-api` into NostrBotKit. It is to add **optional layers** that broaden the user base without giving up the JSON-RPC core, the schema-first contract, the CLI/daemon separation, or the security posture.

Key architectural facts from the current codebase:

- The daemon loads **static** bot identities from `pacto-bot-api.toml` (R5/R7/R9). The daemon never creates or deletes identities; `pacto-bot-admin` does (KTD-8).
- Events are dispatched to **registered handlers** via `handler.register` (R15). The daemon fans out `agent.event` notifications and enforces per-call capabilities on `agent.send_dm`, `agent.set_profile`, and `agent.error` (R14/R16).
- The HTTP transport is already `axum`-based, bound to loopback, and authenticated with `X-Pacto-Bot-Secret` (R2/R3). It currently exposes `POST /` for JSON-RPC and `GET /events` for SSE.
- Persistence is SQLite in WAL mode: `cursors` and `handler_registrations` tables (R19/R20).
- The scaffold generator is template-driven and embeds templates in the binary (`src/scaffold/generate.rs`, `src/scaffold/template.rs`).

These facts make several of NostrBotKit’s features unusually easy to adopt.

---

## 2. Inbound webhooks: `POST /webhook/{bot_id}/{trigger}`

### What NostrBotKit does

NostrBotKit runs a second HTTP server on `webhook_port` (default 0.0.0.0) with:

```rust
.route("/webhook/{bot_id}/{trigger}", post(handle_webhook))
```

- Auth: `Authorization: Bearer <key>` where the key is resolved from a per-bot env var (`webhook_api_key_env`) or a global `NBK_WEBHOOK_API_KEY` env var.
- Body: optional plain text passed as command arguments.
- Query: `?recipient=npub1...` sends the result as a DM to that pubkey.
- Behavior: loads the bot YAML, finds the matching `/trigger` command, executes it via the internal command dispatcher, optionally DM’s the result, and returns the result as plain text.
- Source: `src/webhook.rs` in NostrBotKit.

### Relevant standards and crates

- No Nostr-specific NIP; it is a plain HTTP integration surface.
- `axum` is already in `pacto-bot-api`’s dependencies.
- Secret comparison: `subtle` (already used for the HTTP token) for constant-time bearer comparison.

### How it fits `pacto-bot-api`

Because we have no built-in command catalog, a webhook in our daemon cannot directly invoke a command. But it can reuse the existing **dispatch pipeline**:

- A webhook hit creates a synthetic `WebhookEvent`.
- The daemon converts it into an `agent.event` with a new event type, e.g. `webhook_received`, or dispatches a JSON-RPC notification `agent.webhook` to handlers registered for that `bot_id`.
- Handlers reply with the normal `handler.response` actions (`ack`, `reply`, `defer`, `ignore`).
- The daemon returns the `reply` content as the HTTP response body, or `202 Accepted` if no handler replies.

Alternatively, for operators who do not want to write a handler, a future declarative command layer (see §4) could be the webhook target, making the feature useful even without a custom handler process.

### Implementation sketch

1. Add to `schemas/jsonrpc.json`:
   - Event type `webhook_received` in `handler.register`.
   - Optional notification `agent.webhook` with fields `bot_id`, `trigger`, `body`, `headers` (sanitized), `query_params`.
2. Add a new config section in `pacto-bot-api.toml`:
   ```toml
   [http]
   webhook_enabled = true
   webhook_secret = "env:WEBHOOK_SECRET" # or per-bot
   ```
   Or store per-bot webhook secrets in a separate file with `0o600` permissions.
3. Extend `src/transport/http.rs`:
   - Add `POST /webhook/:bot_id/:trigger` route.
   - Verify bearer token against the configured secret.
   - Look up `bot_id` in `ClientManager` to ensure the bot exists.
   - Build a `WebhookEvent` and call `Dispatch::dispatch_webhook(event)`.
   - Wait for terminal handler responses up to a configurable timeout (e.g., 5 seconds), collect the first `reply`, and return it as `text/plain`.
4. Add rate limiting per webhook path/bot to prevent abuse.
5. Persist webhook deliveries and cursor-like offsets? Probably not for Phase 1; treat webhooks as synchronous best-effort.

### Abilities unlocked

- CI systems, monitoring tools, and external SaaS can trigger bot actions.
- No handler process needs to be running for simple webhook-triggered DMs if combined with the declarative command catalog.
- Bridges the gap between our JSON-RPC world and HTTP-only integrations.

### Effort, risks, and priority

- **Effort:** Low to medium. The HTTP transport and dispatch already exist.
- **Risks:**
  - Synchronous webhooks that wait for handler replies can hang; need a short timeout and proper HTTP status codes.
  - Bearer tokens must be compared in constant time and never logged.
  - Webhooks expose a public-ish surface (even if localhost) — must remain loopback-only or behind a reverse proxy.
- **Priority:** High. It is the cheapest Phase 3 precursor and a common early need.

---

## 3. Outbound `notify_url` callbacks

### What NostrBotKit does

Any `CustomCommand` can declare an optional `notify_url`. After execution, NostrBotKit fires a JSON POST to that URL with:

```json
{
  "trigger": "/weather",
  "sender": "npub1...",
  "args": "berlin",
  "result": "22°C in Berlin"
}
```

This is used to forward command results to analytics, logging, or to chain another service. Source: `src/commands/dispatch.rs` (`fire_notify`).

### Relevant standards and crates

- Plain HTTP callbacks; no Nostr-specific NIP.
- `reqwest` is already in `pacto-bot-api`’s dependencies (with `rustls-tls`).

### How it fits `pacto-bot-api`

We have two natural places to attach `notify_url`:

1. **Per-handler registration:** A handler declares `notify_url` at `handler.register`. The daemon POSTs a copy of every event delivered to that handler, and/or every mutating action the handler takes, to that URL.
2. **Per-event/action:** `agent.send_dm` and `agent.set_profile` accept an optional `notify_url` parameter and fire the callback after the action is published.

Option 1 is closer to NostrBotKit’s model and more useful for event forwarding. Option 2 gives handlers explicit control.

### Implementation sketch

1. Extend `schemas/jsonrpc.json`:
   - Add optional `notify_url` and `notify_headers` to `handler.register` params.
2. Extend `src/handlers.rs`:
   - Add `notify_url: Option<String>` to `HandlerRef`.
3. Extend `src/db.rs`:
   - Add `notify_url` column to `handler_registrations` table (or a new table).
4. In `src/dispatch.rs`:
   - After `dispatch_event` sends an `agent.event`, also fire a POST to `notify_url` if set.
   - After a successful `agent.send_dm` or `agent.set_profile`, fire a POST with event metadata (event id, bot_id, recipient, method, no content to avoid leaking plaintext).
5. Use a bounded channel + background task to avoid blocking the relay publish on a slow HTTP endpoint.

### Abilities unlocked

- Forward all DMs to a SIEM, audit log, or webhook-driven automation.
- Chain bot outputs to another service without writing a custom handler.
- Build integrations where the daemon is the event source and an external system is the consumer.

### Effort, risks, and priority

- **Effort:** Low.
- **Risks:**
  - Outbound HTTP can leak metadata (recipient pubkeys) if not configured carefully.
  - Slow or failing callbacks must not block event dispatch or cursor advancement.
  - Need to redact secrets from `notify_headers` and payloads.
- **Priority:** High. Very low effort, high integration value.

---

## 4. Cron / scheduled jobs

### What NostrBotKit does

NostrBotKit stores per-bot cron jobs in `BotConfig.crons`:

```yaml
bots:
  - bot_id: "daily-bot"
    crons:
      - name: "morning-digest"
        trigger: "/digest"
        cron: "0 9 * * *"
        args: ""
        recipient: "npub1..."
```

A per-bot Tokio task (`src/cron.rs`) wakes at the next minute boundary, evaluates each job in the configured timezone, runs the command, and delivers the result as a DM. It also cleans up expired dialog/menu sessions once per hour.

### Relevant standards and crates

- Cron parsing: `cron` crate (standard Rust cron parser).
- Timezones: `chrono-tz` crate.
- Persistence: SQLite (already used).

### How it fits `pacto-bot-api`

Because our handlers are external processes, cron jobs in our daemon should be **event-driven**, not command-driven. At the scheduled time, the daemon should dispatch a synthetic event to registered handlers, which then decide what to do. This keeps the daemon from being a command interpreter.

However, for reliability and for simple operator use cases, we may also want **built-in cron actions** (e.g., `send_dm` with a static message) that do not require a connected handler.

### Implementation sketch

1. Add config schema for cron jobs:
   ```toml
   [[bots.cron]]
   name = "morning-digest"
   schedule = "0 9 * * *"
   timezone = "America/New_York"
   event_type = "cron_fired"          # or built-in action
   payload = { command = "digest" }
   ```
2. Add a `cron_jobs` table to `src/db.rs`:
   - `bot_id`, `name`, `schedule`, `timezone`, `last_run_at`, `next_run_at`, `payload_json`.
3. Add a `Scheduler` task in `src/scheduler.rs`:
   - Sleep until the next minute boundary.
   - Query jobs whose `next_run_at <= now`.
   - For each due job:
     - If `event_type = "cron_fired"`, dispatch an `AgentEvent` with type `CronFired` to registered handlers.
     - If a built-in action, execute it directly (e.g., `agent.send_dm` with static content).
   - Update `last_run_at` and `next_run_at`.
4. Add `CronFired` to `EventType` in `src/events.rs` and `schemas/jsonrpc.json`.
5. Expose `agent.cron` methods for handlers to acknowledge or defer.

### Abilities unlocked

- Daily digests, hourly health checks, scheduled announcements.
- Time-based bots without keeping a handler process alive or writing polling loops.
- Cron jobs survive daemon restarts because they are persisted in SQLite.

### Effort, risks, and priority

- **Effort:** Medium. Requires a new scheduler task, timezone handling, and persistence.
- **Risks:**
  - If the daemon is down during a scheduled minute, the job may be missed unless we store `last_run_at` and catch up carefully.
  - Timezone configuration mistakes can cause jobs to run at unexpected times.
  - Handler disconnects at cron time mean the event is lost unless combined with built-in actions or persistent redelivery (Phase 2).
- **Priority:** Medium. Common bot need, but requires design care around persistence and delivery semantics.

---

## 5. Declarative command catalog

### What NostrBotKit does

NostrBotKit’s `CommandType` enum covers a broad catalog of built-in commands configured in YAML:

```rust
pub enum CommandType {
    Reply, Post, DmNpub, DmFollowing, DmFollowers, DmGroup,
    ApiCall, RssFeed, Dialog, Menu, Composite, Reaction,
    DmMarmot, DmMarmotNpub,
}
```

Operators create commands via DM commands like `/addcmd trigger=/weather type=api_call url=... template=...`. Commands are stored in per-bot YAML and executed by `src/commands/dispatch.rs`. Source: `src/config.rs` and `src/commands/`.

### Relevant standards and crates

- **RSS:** `rss` or `atom_syndication` crates, or just `reqwest` + an XML parser.
- **Templating:** `minijinja` (NostrBotKit’s choice) or `handlebars`. Our scaffold generator already has a tiny template engine (`src/scaffold/template.rs`), but it is not suitable for runtime templates.
- **HTTP calls:** `reqwest` (already in deps).
- **NIP-92 media tags:** for attaching media URLs to `Kind 1` posts.

### How it fits `pacto-bot-api`

The cleanest fit is to add a **built-in “simple handler”** that registers itself for the declarative bot identities. This preserves our design principles:

- The daemon remains the runtime.
- The JSON-RPC model remains the primary extension point.
- Non-programmers get a YAML layer without forcing programmers to use it.

The simple handler would be an internal daemon component (not an external process) that reads a command catalog and executes actions by calling the same internal functions used by `agent.send_dm` and `agent.set_profile`.

### Implementation sketch

1. Add a new config file or section, e.g. `commands/{bot_id}.toml` or a `[[bots.commands]]` block in `pacto-bot-api.toml`:
   ```toml
   [[bots.commands]]
   trigger = "/weather"
   type = "api_call"
   url = "https://wttr.in/{args}?format=j1"
   template = "{{ current_condition.0.temp_C }}°C in {{ nearest_area.0.areaName.0.value }}"
   deliver = "reply" # reply | post | dm
   ```
2. Add `src/simple_handler/`:
   - `CommandCatalog` loads command definitions.
   - `SimpleHandler` registers for `dm_received` and `cron_fired` events for configured bots.
   - `CommandExecutor` matches the trigger, executes the command type, and dispatches the result via the existing `ClientManager` publish path.
3. Implement command types incrementally:
   - **reply:** static text or templated text.
   - **api_call:** GET/POST URL, parse JSON, render template.
   - **post:** publish a `Kind 1` note (requires extending the publish path beyond DMs).
   - **rss_feed:** poll feed, post new items, store seen GUIDs in SQLite.
   - **composite:** parallel HTTP calls, merged template context.
   - **menu / dialog:** session state stored in SQLite, intercept subsequent messages until complete.
4. Add capability checks: `simple_handler.send_dm`, `simple_handler.post`, etc.
5. Add `pacto-bot-admin add-command` and `pacto-bot-admin remove-command` to edit catalog files safely (refusing to run if the daemon lock is held).

### Abilities unlocked

- Non-programmers can build bots from YAML.
- Common bot patterns (weather, RSS, simple Q&A) work out of the box.
- Broadens the addressable user base from developers to operators.

### Effort, risks, and priority

- **Effort:** Medium to high. The core is easy; menu/dialog session state and RSS item deduplication add real work.
- **Risks:**
  - Template injection and SSRF from user-supplied URLs.
  - RSS polling can be noisy and must be rate-limited.
  - Dialog/menu sessions must be scoped per-sender and time out.
  - We must avoid duplicating handler logic; the simple handler should call the same internal publish paths as JSON-RPC handlers.
- **Priority:** Medium. High value for adoption, but it is a large feature; consider shipping it incrementally (reply/api_call first, then post/RSS, then menu/dialog).

---

## 6. Lightning / NWC integration

### What NostrBotKit does

NostrBotKit supports NWC (NIP-47) via a per-bot or global `nwc_env` config pointing to an env var that holds the `nostr+walletconnect://...` URI. It implements:

- `make_invoice` and `lookup_invoice` using `nostr::nips::nip47` primitives.
- A `PaymentStore` for pending invoices keyed by sender pubkey.
- A background poller that checks invoice settlement every 10 seconds.
- Payment-gated commands: `price_sats` on a `CustomCommand` returns a BOLT11 invoice; once settled, the deferred action executes.
- Zap notifications: `notify_admin_on_zap` sends a DM to the admin when the bot receives a zap (kind 9735).
- `lud16` is published in the bot profile.

Sources: `src/payment.rs`, `src/config.rs`, `src/commands/dispatch.rs`.

### Relevant standards and crates

- **NIP-47:** Nostr Wallet Connect URI, `kind 23194` request / `kind 23195` response / `kind 23197` notification.
- **NIP-57:** Zaps (kind 9735 zap receipts).
- **NIP-47 info event:** kind 13194.
- **Crates:** `nostr` 0.43 already includes `nostr::nips::nip47`. `nostr-sdk` 0.43 does not expose a high-level NWC client, so we would use the low-level primitives like NostrBotKit does.

### How it fits `pacto-bot-api`

Add a **payment backend** alongside the signing backend. A bot identity can optionally have an `nwc` section:

```toml
[[bots]]
id = "premium-bot"
npub = "npub1..."
signing = { backend = "bunker_remote", uri = "bunker://..." }
[nwc]
uri_env = "PREMIUM_BOT_NWC_URI" # or uri_file = "/run/secrets/nwc-uri"
```

The daemon initializes an `NwcClient` per bot when the URI is present. It then:

- Exposes new JSON-RPC methods: `agent.make_invoice`, `agent.pay_invoice`, `agent.lookup_invoice`, `agent.get_balance`.
- Forwards NWC notifications (`payment_received`, `payment_sent`) as `agent.payment` events to handlers.
- Subscribes to kind 9735 zap receipts addressed to the bot and forwards them as `agent.zap_received` events.
- Optionally gates commands in the declarative catalog by `price_sats`.

### Implementation sketch

1. Add `src/payment.rs`:
   - `NwcClient` wrapping `NostrWalletConnectURI` and `nostr::nips::nip47::Request`.
   - `make_invoice`, `pay_invoice`, `lookup_invoice`, `get_balance` methods.
   - Per-operation relay subscription (like NostrBotKit) or a persistent relay connection if we want to receive notifications.
2. Add `PaymentStore` (`Arc<Mutex<HashMap<String, PendingPayment>>>`) for payment-gated actions.
3. Add a background poller for pending invoices.
4. Extend `schemas/jsonrpc.json` with payment methods and events.
5. Extend `BotConfig` and `BotState` with `nwc` backend.
6. Add zap receipt subscription in the Nostr client layer (`src/nostr.rs` or `src/client_manager.rs`).
7. Redact NWC URI from logs and errors (use `secrecy::SecretString`).

### Abilities unlocked

- Premium/paid bot commands.
- Donation and zap notifications.
- Lightning invoices as a bot interaction primitive (e.g., “pay 100 sats to unlock this report”).
- `lud16` profile field support.

### Effort, risks, and priority

- **Effort:** Medium to high. The low-level NWC primitives are fiddly; payment polling requires careful state management.
- **Risks:**
  - NWC URI is a secret; must be stored and handled like `nsec` and bunker URIs.
  - Payment polling can be expensive if done per-user.
  - `pay_invoice` actions must be authorized and rate-limited to prevent draining the wallet.
  - Zaps require reliable relay subscription to kind 9735.
- **Priority:** Medium to high for ecosystem fit. Important for Pacto’s value proposition, but not a prerequisite for the core JSON-RPC model.

---

## 7. Marmot / MLS group chat

### What NostrBotKit does

NostrBotKit already ships Marmot (MLS over Nostr, NIP-104/EE) support via `mdk-core` and `mdk-sqlite-storage`:

- A parallel Tokio task per bot (`src/router/handle_marmot.rs`) listens for `Kind::MlsGroupMessage` events.
- It initializes an `MDK<MdkSqliteStorage>` instance per bot, with a DB encryption key derived from the bot’s secret key.
- It publishes key packages (`kind 443`, `kind 30443`, `kind 10051`) and subscribes to group-specific relays.
- It decrypts MLS welcome messages and group messages, then dispatches them through the same `dispatch()` function used for DMs, so existing commands can reply to groups.
- It supports `DmMarmot` and `DmMarmotNpub` command types to send messages to MLS groups and 1:1 Marmot chats.

Sources: `src/router/handle_marmot.rs`, `src/config.rs` (`CommandType::DmMarmot`, `marmot_relays`).

### Relevant standards and crates

- **NIP-104 / NIP-EE:** E2EE messaging using MLS.
- **Marmot Protocol:** combines Nostr identity with MLS (RFC 9420) for group messaging.
- **Crates:** `mdk-core`, `mdk-sqlite-storage` (not yet on crates.io; must be consumed via git or path dependency, which conflicts with our standalone-repo KTD-1).
- **MLS crate:** `openmls` (underlying `mdk-core`).

### How it fits `pacto-bot-api`

This is explicitly planned for **Phase 2** in our architecture. The daemon would:

- Add a `marmot` boolean and `marmot_relays` to `BotConfig`.
- For each bot with `marmot = true`, spawn a `MarmotTask` that owns an `MDK<MdkSqliteStorage>` instance.
- Subscribe to `kind 443`/`30443`/`10051` key packages and `kind 104`/`MlsGroupMessage` group messages.
- Decrypt incoming messages and dispatch `agent.marmot_message` events to handlers with fields: `group_name`, `sender`, `content`, `event_id`.
- Add `agent.send_marmot_message` JSON-RPC method for handlers to send to groups or 1:1 Marmot chats.
- Store Marmot state in a per-bot SQLite DB under `$DATA_DIR/marmot/{bot_id}/state.db`.

### Implementation sketch

1. Wait for `mdk-core` and `mdk-sqlite-storage` to be published (or accept git path dependencies, which revisits KTD-1).
2. Add `src/marmot.rs` with:
   - `MarmotTask` struct holding `MDK<MdkSqliteStorage>`.
   - `start_marmot_task(bot_config, client_manager, db_encryption_key)`.
3. Derive DB encryption key:
   - For `nsec` backend: derive from secret key via SHA-256 (like NostrBotKit).
   - For bunker backends: require a separate `marmot_db_key` in config or derive from a stable secret shared with the bunker.
4. Add `agent.marmot_message` and `agent.send_marmot_message` to `schemas/jsonrpc.json`.
5. Add event type `MarmotMessageReceived` to `EventType`.
6. Add capability `SendMarmotMessage` to bot capabilities.
7. Update `pacto-dev-env` to include a Marmot-capable relay or test vectors.

### Abilities unlocked

- Bots participating in Pacto squads / MLS groups.
- Group-chat automation (moderation, scheduling, shared digests).
- Pacto’s core differentiator: private group messaging with bots.

### Effort, risks, and priority

- **Effort:** High. Already planned for Phase 2.
- **Risks:**
  - `mdk-core` availability and API stability.
  - DB encryption key for bunker-backed bots is a hard design problem.
  - MLS Welcome payloads and group state are large; the 1 MB frame limit may need reconsideration.
  - Parallel per-bot MDK instances increase memory and CPU usage.
- **Priority:** High, but gated by Phase 2 planning and dependency availability.

---

## 8. Web admin UI

### What NostrBotKit does

NostrBotKit serves a browser-based admin UI at `/admin` when `admin_ui: true` is set in global config. It provides:

- Overview of all bots, running status, start/stop/restart/delete actions.
- Bot detail pages with commands, cron jobs, groups, and profile editing.
- Forms to add/remove commands, cron jobs, groups, and blueprints.
- Media upload page at `/media`.
- No built-in password; documentation says to put it behind a reverse proxy.

Sources: `src/admin.rs`, `src/webhook.rs` (route registration), `static/` assets.

### How it fits `pacto-bot-api`

Our daemon already has an HTTP transport. We could add a minimal admin UI to the same port, but our design deliberately separates lifecycle operations into `pacto-bot-admin`. The daemon should **not** create or delete bot identities (KTD-8). Therefore, the admin UI should be **read-only or limited to safe runtime actions**:

- View daemon status, metrics, and registered handlers.
- Trigger HTTP token rotation (delegates to the same logic as `pacto-bot-admin rotate-http-token`).
- View logs/diagnostics (read-only).
- NOT create/delete bots, NOT edit signing config, NOT expose secrets.

A richer UI for creating bots and commands belongs in `pacto-bot-admin` as a local web interface, not in the daemon. But the daemon UI is still useful for operators who want a dashboard.

### Implementation sketch

1. Add `admin_ui_enabled = true` under `[http]` in `pacto-bot-api.toml`.
2. Add axum routes in `src/transport/http.rs` or a new `src/admin_ui.rs`:
   - `GET /admin` -> HTML dashboard.
   - `GET /admin/metrics` -> JSON metrics.
3. Embed static HTML/CSS/JS with `include_dir` (same pattern as scaffold templates).
4. Use the existing `X-Pacto-Bot-Secret` for authentication (or a separate admin secret).
5. Expose read-only data from `Diagnostics`, `ClientManager`, and `HandlerRegistry`.

### Abilities unlocked

- Visual dashboard for operators who prefer GUIs over CLI.
- Easier onboarding for non-technical operators.
- Quick health check without running `pacto-bot-admin status`.

### Effort, risks, and priority

- **Effort:** Medium. HTML/CSS/JS bundling and security review take time.
- **Risks:**
  - Increases daemon attack surface.
  - No built-in password means it must be behind a reverse proxy; operators will misconfigure it.
  - Writing actions in the daemon risks violating KTD-8.
- **Priority:** Medium. Operator convenience, but the CLI-first model is safer.

---

## 9. Blueprint / template system

### What NostrBotKit does

NostrBotKit blueprints are parametrized YAML templates. A blueprint has a `params:` block and a template body with `{{PARAM_NAME}}` placeholders. During setup, the operator answers prompts; the placeholders are replaced; the resulting YAML is applied to a new `BotConfig`. Sources: `src/blueprint.rs`, `src/config.rs` (`BlueprintParam`, `PendingSetup`).

### How it fits `pacto-bot-api`

Our scaffold generator is already a template system, but it runs at **project generation time** and produces Python handler projects. We can extend this idea to **bot identity creation time**:

- `pacto-bot-admin new --template weather-bot --bot-id my-weather-bot` loads a blueprint, prompts for parameters, and writes the resolved bot config snippet into `pacto-bot-api.toml`.
- This keeps identity creation in the admin CLI (KTD-8 compliant) and gives operators reusable templates.
- We could also have command-catalog blueprints that generate `commands/{bot_id}.toml` for the declarative layer (§4).

### Implementation sketch

1. Add `templates/bots/` directory with `.toml` or `.yaml` blueprint files.
2. Define `Blueprint` schema:
   ```yaml
   params:
     - name: CITY
       prompt: "Which city?"
       validate: "^[A-Za-z ]+$"
   id: "{{BOT_ID}}"
   npub: "" # operator fills or generates
   commands:
     - trigger: "/weather"
       type: "api_call"
       url: "https://wttr.in/{{CITY}}?format=j1"
       template: "..."
   ```
3. Add `pacto-bot-admin new --template <name>` flow:
   - Load blueprint.
   - Prompt for params (reuse existing `prompt_*` helpers in `src/admin.rs`).
   - Replace `{{BOT_ID}}` and param placeholders.
   - Append the resolved bot config to `pacto-bot-api.toml`.
4. Reuse the existing scaffold template engine or adopt a more capable one like `minijinja`.

### Abilities unlocked

- Shareable bot templates (e.g., “support bot,” “weather bot”).
- Faster onboarding for common bot patterns.
- Community-driven template library without changing the daemon.

### Effort, risks, and priority

- **Effort:** Medium. Mostly CLI work; overlaps with scaffold generator.
- **Risks:**
  - Template injection if placeholders end up in config fields that are later executed.
  - Need to validate resolved config against `schemas/config.json` before writing.
- **Priority:** Medium. Good companion to the declarative command catalog.

---

## 10. Related features not in the top eight

### 10.1 Media uploads (NIP-96 / Blossom)

NostrBotKit supports media uploads via `POST /media/upload`, proxying to a NIP-96 server or Blossom server with NIP-98 auth. This is useful for bots that post images or videos. For `pacto-bot-api`, this would mean:

- Adding `agent.upload_media` JSON-RPC method.
- Storing NIP-96/Blossom server URLs in bot config.
- Building NIP-98 auth events (`kind 27235`) with the bot’s signer.
- Returning the public URL so handlers can include it in `agent.send_dm` or `agent.post` content.

Effort: medium. Risk: file uploads over localhost HTTP, content validation, and size limits. Priority: medium, tied to the `post` command type and general media bots.

### 10.2 DM-first runtime control (AlphaBot)

NostrBotKit’s AlphaBot can create, start, stop, and delete bots via DM. This directly conflicts with our KTD-8 decision that the daemon never creates or deletes identities. We should **not** adopt this. Remote administration of runtime state can be achieved via `pacto-bot-admin` over SSH or a future local admin UI, not over the Nostr DM surface.

---

## 11. Implementation ordering recommendation

| Rank | Feature | Effort | Priority | Why |
|------|---------|--------|----------|-----|
| 1 | Outbound `notify_url` callbacks | Low | High | Tiny change, huge integration value, reuses existing HTTP client. |
| 2 | Inbound webhooks | Low-Medium | High | Reuses existing HTTP transport and dispatch; Phase 3 precursor. |
| 3 | Human-readable config export (already partially done via `export`) | Low | Medium | Operational backup/inspection; keeps SQLite as runtime. |
| 4 | Cron / scheduled jobs | Medium | Medium | Common bot need; requires scheduler persistence. |
| 5 | Blueprint / template system | Medium | Medium | CLI-only; no daemon changes; pairs with declarative catalog. |
| 6 | Declarative command catalog (reply, post, api_call, RSS, composite) | Medium-High | Medium | Broadens user base; ship incrementally. |
| 7 | Web admin UI (read-only) | Medium | Medium | Operator convenience; keep write operations in CLI. |
| 8 | Lightning / NWC | Medium-High | Medium-High | Ecosystem fit; defer until Phase 2 planning stabilizes. |
| 9 | Marmot / MLS group chat | High | High (Phase 2) | Dependency- and design-gated; must-study NostrBotKit implementation. |
| 10 | Media uploads (NIP-96/Blossom) | Medium | Medium | Pair with declarative `post` command type. |
| — | DM-first runtime control | — | Non-goal | Conflicts with KTD-8; avoid. |

### Suggested first slice

A single PR that delivers items 1 and 2 together:

1. Add `notify_url` to `handler.register` and fire callbacks on events and mutating actions.
2. Add `POST /webhook/:bot_id/:trigger` to the HTTP transport, dispatching `agent.webhook` events to handlers and returning the first reply.
3. Add corresponding `pacto-bot-admin` CLI commands to rotate webhook secrets and test webhooks.
4. Update `schemas/jsonrpc.json`, regenerate types, and add in-process tests with `reqwest` mocking.

This slice is low-risk, high-value, and proves the pattern for later inbound/outbound HTTP features.

---

## 12. Sources

- NostrBotKit source: https://codeberg.org/Tuxor/NostrBotKit (main branch, `nbk` v0.5.2)
  - `src/webhook.rs` — inbound webhook endpoint.
  - `src/cron.rs` — scheduled job runner.
  - `src/config.rs` — `CommandType`, `CustomCommand`, `CronJob`, `BlueprintParam`.
  - `src/commands/dispatch.rs` — command dispatcher, `notify_url`, payment gating.
  - `src/payment.rs` — NWC client, invoice polling, payment store.
  - `src/router/handle_marmot.rs` — Marmot/MLS integration.
  - `src/admin.rs` — web admin UI.
  - `src/blueprint.rs` — blueprint loading and resolution.
  - `src/media.rs` — NIP-96 / Blossom media upload proxy.
- `pacto-bot-api` local repository:
  - `src/transport/http.rs` — existing HTTP transport.
  - `src/handlers.rs` — handler registry and capability model.
  - `src/dispatch.rs` — event dispatch and rate limiting.
  - `src/scaffold/generate.rs` — template-driven project generation.
  - `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` — requirements R1–R37 and KTDs.
  - `schemas/jsonrpc.json` — canonical JSON-RPC catalog.
- External standards:
  - NIP-47: https://nips.nostr.com/47
  - NIP-96: https://nips.nostr.com/96
  - NIP-104 / NIP-EE (Marmot/MLS): https://github.com/marmot-protocol/marmot
  - Marmot Development Kit: https://github.com/marmot-protocol/mdk
  - NWC guide: https://rust-nostr.org/sdk/nips/47.html

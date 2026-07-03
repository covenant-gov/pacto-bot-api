# nip-lookup

Use this skill when the user asks about a specific Nostr Implementation Possibility (NIP) — e.g., "what is NIP-72?", "explain NIP-46", or "how do NIP-59 gift wraps work?" — in the context of the `pacto-bot-api` daemon.

This skill is **not** for generic Nostr protocol questions that don't name a NIP, nor for implementing unsupported NIPs without an explicit plan.

## Trigger phrases

- "what is NIP-XX"
- "explain NIP-XX"
- "how does NIP-XX work"
- "NIP-72"
- "NIP-46"
- "NIP-59"
- "NIP-44"
- "NIP-17"
- "NIP-01"
- "NIP-65"
- "NIP-90"
- "NIP-29"

## Disambiguation

| User says | Load this skill? |
|---|---|
| "what is NIP-72?" | Yes |
| "explain NIP-46" | Yes |
| "how do gift wraps work?" | Yes — maps to NIP-59 |
| "explain Nostr" | No — too generic |
| "what is a relay?" | No — generic Nostr concept |
| "implement NIP-72" | Yes — but also escalate to planning because NIP-72 is `unrecommended` |

## How to answer any NIP question

For the requested NIP, produce a concise answer covering the **5 Ws and 1 H**:

| Question | What to cover |
|---|---|
| **What** | The protocol, data structure, or behavior the NIP defines. |
| **Who** | The actors (author, recipient, signer, relay, moderator, community owner, etc.). |
| **When** | Event lifecycle: creation, publication, replacement, expiration, ordering. |
| **Where** | Where data lives or is transmitted (relay, kind, tag, bunker, DM, gift wrap). |
| **Why** | The problem the NIP solves; why a bot or daemon would care. |
| **How** | Wire format, event kinds, tags, encryption/encoding, and how `pacto-bot-api` uses or could use it. |

After the 5W1H, add **2–3 concrete use cases** tied to `pacto-bot-api` files, commands, or architecture.

Canonical specs: `https://github.com/nostr-protocol/nips/blob/master/<NIP>.md`.

## NIP quick-reference for pacto-bot-api

### NIP-01 — Basic protocol
- **What**: Defines the `Event` structure, signatures, kinds, tags, relay `REQ`/`EVENT`/`CLOSE` messages, and subscriptions.
- **Who**: Event authors, relays, and subscribers.
- **When**: Events are created once, signed, published, and matched by filters.
- **Where**: Raw events live on relays; `kind` and tags determine routing.
- **Why**: Foundation for every Nostr interaction the daemon performs.
- **How**: `nostr-sdk` `Client` creates/subscribes/publishes; the daemon filters incoming traffic by `kind` and `#p`.
- **Use cases in codebase**: `src/client_manager.rs` subscribes each bot to relay filters; `src/nostr.rs` publishes events.

### NIP-04 — Encrypted DMs (deprecated)
- **What**: Kind `4` events encrypted with a secp256k1 shared secret.
- **Who**: Sender and recipient.
- **When**: Published and decrypted like a normal event.
- **Where**: On relays as `kind:4`.
- **Why**: Early direct-messaging standard.
- **How**: Superseded by NIP-17/44/59 for new work; the daemon does not use NIP-04 for handler messages.
- **Use cases in codebase**: Legacy compatibility only; not used for new bots.

### NIP-17 — Private Direct Messages
- **What**: Semantics for 1:1 private DMs using NIP-59 gift wraps and NIP-44 seals.
- **Who**: Two parties exchanging private messages.
- **When**: A rumor is sealed and wrapped before publishing; the recipient unwraps and decrypts.
- **Where**: Inner rumor is hidden inside a `kind:1059` gift wrap.
- **Why**: Metadata-minimal direct messaging between bots and users.
- **How**: `src/nostr.rs` sends NIP-17 DMs as NIP-59 gift wraps and decrypts incoming gift wraps into `AgentEvent`.
- **Use cases in codebase**: Bot handler receives `agent.event` of type `dm_received`; handler replies with `agent.send_dm`.

### NIP-44 — Encrypted Payloads
- **What**: Versioned authenticated-encryption format for Nostr messages.
- **Who**: Any two parties with nostr keys.
- **When**: Used to encrypt the inner seal and rumor before wrapping in a NIP-59 gift wrap.
- **Where**: Inside `kind:13` seals and inside `kind:1059` gift wraps.
- **Why**: Stronger, versioned encryption than NIP-04.
- **How**: `nostr-sdk` provides `nip44::encrypt`/`decrypt`; the daemon uses it in `src/nostr.rs` and `src/signer.rs`.
- **Use cases in codebase**: `src/signer.rs` exposes `nip44_encrypt`/`nip44_decrypt` across all signer backends; `src/nostr.rs` wraps outbound DMs.

### NIP-46 — Nostr Connect (bunker)
- **What**: Remote-signer protocol where a "bunker" holds the private key and signs events on behalf of a client.
- **Who**: Daemon (client) + remote NIP-46 bunker (signer).
- **When**: Signature is requested per event; policies can restrict event kinds.
- **Where**: Over `bunker://` URI or Nostr relay messages.
- **Why**: Bot process never holds the raw `nsec`; supports hardware/cold signers.
- **How**: Configured in `pacto-bot-api.toml` as `backend = "bunker_local"` or `"bunker_remote"`; `src/nip46.rs` verifies the bunker's live pubkey; `src/signer.rs` uses `nostr_connect` for encryption/signing.
- **Use cases in codebase**: Production signing backend; `pacto-bot-admin test-bunker` validates connectivity and pubkey match.

### NIP-59 — Gift Wraps
- **What**: `kind:1059` events that hide the true sender and recipient using ephemeral keys and NIP-44 encryption.
- **Who**: Sender wraps a sealed rumor for a recipient; only the recipient can unwrap.
- **When**: Published once per recipient; recipient's client unwraps to reveal the inner rumor.
- **Where**: On relays as `kind:1059`.
- **Why**: Hides conversation metadata from relays.
- **How**: The daemon subscribes to `kind:1059` events addressed to the bot's pubkey (`#p`), decrypts via `src/nostr.rs`, and forwards the inner event as an `agent.event` notification.
- **Use cases in codebase**: Receiving inbound DMs; sending outbound DMs.

### NIP-65 — Relay List Metadata
- **What**: `kind:10002` event listing preferred relays for a pubkey.
- **Who**: Any pubkey.
- **When**: Published/updated as a replaceable event.
- **Where**: On relays.
- **Why**: Helps clients discover which relays to query for a pubkey.
- **How**: Not yet used by the daemon; could be read to augment the static `relays` list in `pacto-bot-api.toml`.
- **Use cases in codebase**: Future dynamic relay discovery; bot profile tooling.

### NIP-72 — Moderated Communities (unrecommended)
- **What**: Defines `kind:34550` community definitions, `kind:1111` posts, and `kind:4550` moderator approvals.
- **Who**: Community owner, moderators, posters, and readers.
- **When**: Community definition is replaceable; posts need moderator approval events to be visible.
- **Where**: On relays; community identifier is an `a` tag `34550:<owner-pubkey>:<d-identifier>`.
- **Why**: Topic-specific, moderated spaces on Nostr.
- **How**: A bot could own a community, publish `kind:34550`, and issue `kind:4550` approvals; posts are `kind:1111` with `A`/`a` tags scoped to the community.
- **Use cases in codebase**:
  - A bot acts as a community owner and publishes a `kind:34550` definition for a project support forum.
  - A bot moderator issues `kind:4550` approval events for on-topic posts.
  - A bot cross-posts important announcements from a `kind:1111` thread to its subscribers via DM.
- **Note**: NIP-72 is marked `draft`/`unrecommended`. Prefer **NIP-29** for new public-community designs.

### NIP-29 — Public Chat (recommended over NIP-72)
- **What**: Defines `kind:39000` group metadata, `kind:9` group chat messages, and `kind:39001` membership lists.
- **Who**: Group admins, members, and relays.
- **When**: Metadata is replaceable; messages are append-only.
- **Where**: On relays; group id is an `a` tag `39000:<admin-pubkey>:<d>`.
- **Why**: Simpler public group chat than NIP-72.
- **How**: A bot could be a group admin, manage membership via `kind:39001`, and broadcast `kind:9` messages.
- **Use cases in codebase**: Public announcement channel bot; project support group moderation.

### NIP-90 — Data Vending Machine
- **What**: Public compute bots that publish `kind:31990` capability ads, receive `kind:5xxx` requests, and reply with `kind:6xxx` results.
- **Who**: DVM operator, requesters.
- **When**: Ads are replaceable; requests and results are published publicly.
- **Where**: On relays.
- **Why**: Public, discoverable bot services.
- **How**: A Pacto bot could advertise public services via DVM while keeping private interactions in NIP-17 DMs.
- **Use cases in codebase**: Public feed aggregation, tag search, or public squad discovery bot.

## Anti-patterns

- Do not confuse NIP-04 (legacy DMs) with NIP-17/44/59 (gift-wrap DMs).
- Do not treat NIP-72 as the canonical public-community solution; NIP-29 is preferred.
- Do not assume the daemon implements every NIP listed here. Core runtime coverage is NIP-01, NIP-44, NIP-46, NIP-59, and NIP-17 semantics.
- Do not invent NIP numbers or kind values; verify against the canonical NIPs repo.

## Verification checklist

- [ ] NIP number and status (draft/final, recommended/unrecommended) are correct.
- [ ] 5 Ws and How are covered for the requested NIP.
- [ ] At least one use case ties back to `pacto-bot-api` code, config, or CLI.
- [ ] Any Pacto implementation details reference real files (`src/nostr.rs`, `src/signer.rs`, `src/nip46.rs`, `src/client_manager.rs`).

# kind-lookup

Use this skill when the user asks about a specific Nostr event **kind** — e.g., "what is kind 0?", "explain kind 1059", "how does kind 9 work?", or "what kind should I use for X?" — in the context of the `pacto-bot-api` daemon.

This skill is **not** for generic Nostr protocol questions that don't name a kind, nor for implementing unsupported event kinds without an explicit plan.

## Trigger phrases

- "what is kind X"
- "explain kind X"
- "how does kind X work"
- "kind 0"
- "kind 1059"
- "kind 9"
- "kind 31990"
- "kind 34550"
- "what kind should I use for"

## Disambiguation

| User says | Load this skill? |
|---|---|
| "what is kind 0?" | Yes |
| "explain kind 1059" | Yes |
| "how does kind 9 work?" | Yes |
| "what kind should I use for a profile?" | Yes — maps to kind 0 |
| "what kind should I use for a DM?" | Yes — maps to NIP-17 / kind 1059 gift wraps |
| "explain Nostr" | No — too generic |
| "what is a relay?" | No — generic Nostr concept |
| "implement NIP-72" | No — use `nip-lookup` for NIP questions, then escalate to planning |

## How to answer any kind question

For the requested kind, produce a concise answer covering the **5 Ws and 1 H**:

| Question | What to cover |
|---|---|
| **What** | The event payload, content, tags, and semantics the kind defines. |
| **Who** | The actors (author, recipient, signer, relay, group admin, moderator, DVM operator, etc.). |
| **When** | Event lifecycle: regular vs replaceable vs ephemeral vs parameterized replaceable; creation, replacement, expiration, ordering. |
| **Where** | Where the event lives or is transmitted (relay, kind, tag, gift wrap, group, community). |
| **Why** | The problem the kind solves; why a bot or daemon would care. |
| **How** | Wire format, required/recommended tags, encryption/encoding, and how `pacto-bot-api` uses or could use it. |

After the 5W1H, add **2–3 concrete use cases** tied to `pacto-bot-api` files, commands, or architecture.

Canonical NIPs repo: `https://github.com/nostr-protocol/nips`.

## Kind ranges

| Range | Category | Replacement behavior |
|---|---|---|
| `0–9999` | Regular events | None; each event is distinct. |
| `10000–19999` | Replaceable events | Newer replaces older for the same pubkey + kind. |
| `20000–29999` | Ephemeral events | Not stored by relays; delivered only to live subscriptions. |
| `30000–39999` | Parameterized replaceable events | Newer replaces older for the same pubkey + kind + `d` tag. |

## Kind quick-reference for pacto-bot-api

### kind:0 — Set Metadata (NIP-01)

- **What**: A replaceable event carrying profile metadata (`name`, `about`, `picture`, `display_name`, etc.) as JSON in the content field.
- **Who**: Any pubkey; in Pacto, each bot identity owns its own kind:0.
- **When**: Replaceable (`10000–19999` range); newer kind:0 events supersede older ones for the same pubkey.
- **Where**: Published to and stored on relays.
- **Why**: Lets users discover a bot's name, avatar, and description before interacting.
- **How**: `pacto-bot-admin publish-profile` builds and publishes a kind:0 from config; `src/nostr.rs` handles publish. Handlers may also update metadata via `agent.set_profile`.
- **Use cases in codebase**:
  - `pacto-bot-admin publish-profile my-bot` writes the bot's public profile to relays.
  - A handler calls `agent.set_profile` to change the bot's avatar or description based on runtime state.

### kind:3 — Contact List (NIP-02)

- **What**: A replaceable event listing pubkeys the author follows, with optional relay hints in `p` tags.
- **Who**: Any pubkey maintaining a social graph.
- **When**: Replaceable; updated whenever the author changes who they follow.
- **Where**: On relays.
- **Why**: Defines a pubkey's social graph and can act as an allow-list or discovery signal.
- **How**: Not currently consumed by the daemon; could be read to build a bot-specific allow-list or to discover operator pubkeys.
- **Use cases in codebase**:
  - Future feature: restrict `agent.send_dm` replies so the bot only responds to pubkeys in its contact list.
  - Diagnostics (`pacto-bot-admin diagnose`) could optionally report the bot's contact-list size.

### kind:4 — Encrypted Direct Message (legacy) (NIP-04)

- **What**: A regular event encrypting a message with a secp256k1 shared secret.
- **Who**: Sender and recipient.
- **When**: Published once; recipient decrypts on receipt.
- **Where**: On relays as `kind:4`.
- **Why**: Early direct-messaging standard.
- **How**: Superseded by NIP-17/44/59; the daemon does not use kind:4 for handler messages.
- **Use cases in codebase**:
  - Legacy compatibility only; not used for new bots.
  - A future migration tool might read old kind:4 DMs and re-send them as NIP-17 gift wraps.

### kind:9 — Public Chat Message (NIP-29)

- **What**: A regular group chat message inside a NIP-29 public chat group.
- **Who**: Group members; group admins define membership and rules.
- **When**: Append-only within the group; referenced by group `a` tag.
- **Where**: On relays; scoped to a group identifier such as `39000:<admin-pubkey>:<d>`.
- **Why**: Public, moderated group chat for communities.
- **How**: A bot can publish kind:9 messages as a group member or admin; not yet implemented in the daemon.
- **Use cases in codebase**:
  - A project-announcement bot posts kind:9 messages to a public support group.
  - A handler translates incoming DMs into kind:9 replies when the user is asking in a group context.

### kind:13 — Seal (NIP-59)

- **What**: An encrypted wrapper containing a signed **rumor** (the inner unsigned event).
- **Who**: Sender creates a seal for a specific recipient.
- **When**: Created as an intermediate step before wrapping in a kind:1059 gift wrap.
- **Where**: Embedded inside a kind:1059 gift wrap; never published directly.
- **Why**: Hides the true sender's key from the outer gift-wrap metadata.
- **How**: `nostr-sdk` creates the seal as part of `nip59::gift_wrap`; `src/nostr.rs` relies on this flow for outbound NIP-17 DMs.
- **Use cases in codebase**:
  - Every outbound `agent.send_dm` transits through a kind:13 seal inside a kind:1059 gift wrap.
  - Every inbound kind:1059 is unwrapped into a kind:13 seal and then into the inner rumor.

### kind:1059 — Gift Wrap (NIP-59)

- **What**: A regular event that hides the true sender and recipient using an ephemeral key pair and NIP-44 encryption.
- **Who**: Sender wraps a sealed rumor for a recipient; only the recipient can unwrap.
- **When**: Published once per message; recipient's client unwraps to reveal the inner rumor.
- **Where**: On relays as `kind:1059`.
- **Why**: Metadata-minimal direct messaging; relays cannot see who is talking to whom.
- **How**: The daemon subscribes to `kind:1059` events addressed to the bot's pubkey (`#p`), decrypts them in `src/nostr.rs`, and forwards the inner event as an `agent.event` notification.
- **Use cases in codebase**:
  - Receiving inbound DMs: `src/client_manager.rs` sets up the relay subscription; `src/nostr.rs` decrypts and dispatches.
  - Sending outbound DMs: handler calls `agent.send_dm`; the daemon wraps the reply as kind:1059 and publishes it.

### kind:10002 — Relay List Metadata (NIP-65)

- **What**: A replaceable event listing preferred read/write relays for a pubkey.
- **Who**: Any pubkey.
- **When**: Replaceable; updated when the author changes relay preferences.
- **Where**: On relays.
- **Why**: Helps clients discover which relays to query for a pubkey.
- **How**: Not yet used by the daemon; could be read to augment or override the static `relays` list in `pacto-bot-api.toml`.
- **Use cases in codebase**:
  - Dynamic relay discovery: the daemon could read kind:10002 from the bot's operator and add relays at startup.
  - `pacto-bot-admin diagnose` could report whether the bot's configured relays match its published NIP-65 list.

### kind:1111 — Community Post (NIP-72)

- **What**: A post inside a moderated community; requires a kind:4550 moderator approval event to be visible.
- **Who**: Poster, community moderators, community owner.
- **When**: Published; stays pending until a moderator issues kind:4550.
- **Where**: On relays; scoped to a community `a` tag `34550:<owner-pubkey>:<d-identifier>`.
- **Why**: Topic-specific, moderated public content.
- **How**: A bot could publish kind:1111 posts or act as a moderator; not yet implemented in the daemon.
- **Use cases in codebase**:
  - A bot cross-posts important announcements from a kind:1111 thread to its subscribers via DM.
  - A bot publishes support answers as kind:1111 posts in a community it owns.

### kind:30078 — Arbitrary Application-Specific Data (NIP-78)

- **What**: A parameterized replaceable event for application-specific state (`d` tag names the app/namespace).
- **Who**: Application or bot.
- **When**: Parameterized replaceable; newer versions supersede older ones with the same `d` tag.
- **Where**: On relays.
- **Why**: Lets bots store public, namespaced state without colliding with other event kinds.
- **How**: Not currently used by the daemon; could store bot configuration or public state keyed by a `d` tag.
- **Use cases in codebase**:
  - A bot publishes its public command manifest as kind:30078 under `d=pacto-bot-api:commands`.
  - A multi-instance handler uses kind:30078 to synchronize state across replicas.

### kind:31990 — Data Vending Machine Advertisement (NIP-90)

- **What**: A replaceable event advertising a DVM service (input kinds, output kinds, pricing, etc.).
- **Who**: DVM operator.
- **When**: Replaceable; updated when capabilities change.
- **Where**: On relays.
- **Why**: Makes public compute services discoverable.
- **How**: A bot could publish kind:31990 to advertise public services while keeping private interactions in NIP-17 DMs.
- **Use cases in codebase**:
  - A Pacto bot advertises a public feed-aggregation DVM via `pacto-bot-admin publish-profile` or a handler-published event.
  - A handler receives DVM requests via DM and publishes public results on the user's behalf.

### kind:34550 — Community Definition (NIP-72)

- **What**: A replaceable event defining a moderated community (name, description, rules, moderators).
- **Who**: Community owner.
- **When**: Replaceable; updated when community rules change.
- **Where**: On relays; identified by `d` tag and referenced by `a` tag `34550:<owner-pubkey>:<d>`.
- **Why**: Creates a topic-specific, moderated space on Nostr.
- **How**: A bot could own a community by publishing kind:34550; not yet implemented in the daemon.
- **Use cases in codebase**:
  - A bot acts as a community owner and publishes a kind:34550 definition for a project support forum.
  - A bot updates community rules via `agent.set_profile`-style community metadata updates.

### kind:39000 — Group Metadata (NIP-29)

- **What**: A replaceable event defining a NIP-29 public chat group (name, description, rules, admins).
- **Who**: Group admin.
- **When**: Replaceable; updated when group settings change.
- **Where**: On relays; identified by `d` tag and referenced by `a` tag `39000:<admin-pubkey>:<d>`.
- **Why**: Simpler public group chat than NIP-72 communities.
- **How**: A bot could be a group admin, publish kind:39000, and broadcast kind:9 messages.
- **Use cases in codebase**:
  - A public announcement-channel bot owns a kind:39000 group and publishes kind:9 broadcasts.
  - A handler administers group membership by publishing kind:39001.

### kind:39001 — Group Members (NIP-29)

- **What**: A replaceable event listing members of a NIP-29 group.
- **Who**: Group admin.
- **When**: Replaceable; updated as members join or leave.
- **Where**: On relays.
- **Why**: Controls who can participate in a public chat group.
- **How**: A bot could manage membership via kind:39001; not yet implemented in the daemon.
- **Use cases in codebase**:
  - A bot auto-approves membership requests for a support group.
  - A bot removes inactive members from its announcement group.

### kind:4550 — Moderator Approval (NIP-72)

- **What**: A regular event approving a kind:1111 post inside a moderated community.
- **Who**: Community moderator.
- **When**: Published after reviewing a kind:1111 post; references the post by `e` tag.
- **Where**: On relays.
- **Why**: Gates content visibility in NIP-72 communities.
- **How**: A bot moderator could issue kind:4550 approvals; not yet implemented in the daemon.
- **Use cases in codebase**:
  - A bot moderator issues kind:4550 approval events for on-topic posts in a community it owns.
  - A handler auto-approves posts that match a configured allow-list.

### DVM request/result kinds (kind:5xxx / kind:6xxx) (NIP-90)

- **What**: Regular events requesting (kind:5xxx) or returning (kind:6xxx) data vending machine results.
- **Who**: DVM requester and DVM operator.
- **When**: Requests are append-only; results reference the request.
- **Where**: On relays.
- **Why**: Public, discoverable compute requests and results.
- **How**: A Pacto bot could consume DVM results or act as a DVM and publish results.
- **Use cases in codebase**:
  - A bot handler receives a DM, sends a kind:5xxx DVM request on the user's behalf, and DMs back the kind:6xxx result.
  - A bot publishes public kind:6xxx results for its own compute service.

## Anti-patterns

- Do not confuse regular, replaceable, ephemeral, and parameterized replaceable lifecycles. A kind's range determines whether relays deduplicate or replace it.
- Do not confuse legacy kind:4 DMs with NIP-17 DMs built on kind:1059 gift wraps and kind:13 seals.
- Do not treat NIP-72 as the canonical public-community solution; NIP-29 is preferred for new public-group designs.
- Do not assume the daemon implements every kind listed here. Core runtime coverage is kind:0, kind:13, kind:1059, and the NIP-17 DM semantics built on them.
- Do not invent kind numbers or NIP mappings; verify against the canonical NIPs repo.

## Verification checklist

- [ ] Kind number and range (regular/replaceable/ephemeral/parameterized) are correct.
- [ ] Relevant NIP reference and status (draft/final, recommended/unrecommended) are correct.
- [ ] 5 Ws and How are covered for the requested kind.
- [ ] At least one use case ties back to `pacto-bot-api` code, config, or CLI.
- [ ] Any Pacto implementation details reference real files (`src/nostr.rs`, `src/signer.rs`, `src/nip46.rs`, `src/client_manager.rs`).

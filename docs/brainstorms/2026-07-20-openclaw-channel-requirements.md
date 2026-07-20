---
date: 2026-07-20
topic: openclaw-channel
---

# OpenClaw Nostr Channel Plugin

## Summary

An OpenClaw channel plugin that treats Nostr as another chat platform by proxying through a `pacto-bot-api` daemon. Each channel account maps to one Pacto bot identity and supports text DMs and text MLS group messages for Squads that bot already belongs to.

## Problem Frame

OpenClaw connects to many chat apps, but Nostr is not one of them. Running a Nostr bot requires relay management, NIP-17/44/59 DM encryption, NIP-46 signing, and key hygiene. The Pacto daemon already owns all of that. A channel plugin that registers as a Pacto handler would let OpenClaw users message Nostr pubkeys and existing Squads without duplicating Nostr infrastructure inside OpenClaw.

## Key Decisions

- **Native OpenClaw channel plugin.** Implement the plugin against `openclaw/plugin-sdk/channel-outbound` so Nostr conversations are normalized into OpenClaw's session, routing, and policy model. A Python SDK bridge would be easier to prototype but would sit outside OpenClaw's channel abstractions.
- **One channel account per Pacto bot identity.** The daemon's config is built around static bot identities; mapping one account to one identity keeps authentication and routing simple.
- **Squad membership is out-of-band.** Creating or inviting the bot into a Squad is an admin operation; the plugin only sends and receives messages in Squads the bot already belongs to.
- **Text-only for v1.** DMs and group text are the highest-value chat primitives. Partial support for media, reactions, or zaps would create a fragmented experience.
- **Unix socket as default transport, HTTP as fallback.** The plugin is intended for self-hosted deployments where the daemon and OpenClaw gateway can share a host; HTTP is available for remote or containerized setups.
- **Profile display comes from the daemon.** The plugin should show the bot identity's name and picture as defined by the daemon-managed `kind:0` profile. Operator overrides can be added later if needed.

## Actors

- **A1. OpenClaw operator.** Configures the Pacto daemon, creates the bot identity with `pacto-bot-admin`, and adds the channel account in OpenClaw.
- **A2. OpenClaw end user.** Sends and receives messages with Nostr pubkeys and Squads through OpenClaw's chat UI.
- **A3. Pacto daemon.** Owns the bot identity, relay connections, Nostr encryption, and handler registry.
- **A4. Nostr user.** A person on any Nostr client who sends DMs to the bot identity or participates in a Squad the bot is in.

## Key Flows

- **F1. Send a DM to a Nostr user.**
  - **Trigger:** A2 sends a message to a Nostr pubkey contact in OpenClaw.
  - **Actors:** A2, A3, A4.
  - **Steps:**
    1. OpenClaw resolves the route to the channel account and recipient npub.
    2. The plugin calls `agent.send_dm` with `bot_id`, `recipient`, and `content`.
    3. The daemon encrypts and publishes a NIP-17/NIP-59 DM.
    4. The daemon returns the event id; OpenClaw records the receipt.

- **F2. Receive a DM from a Nostr user.**
  - **Trigger:** A3 receives a `kind:1059` gift wrap addressed to the bot identity.
  - **Steps:**
    1. The daemon decrypts the DM and fans out an `agent.event: dm_received` notification.
    2. The plugin maps `chat_id` (sender npub) to an OpenClaw conversation.
    3. The plugin normalizes the message into OpenClaw's message context and dispatches it.

- **F3. Receive an MLS Squad group message.**
  - **Trigger:** A3 receives an `agent.event: mls_group_message_received` for a Squad the bot belongs to.
  - **Steps:**
    1. The plugin maps `chat_id` (Squad wire id) to an OpenClaw group conversation.
    2. The plugin dispatches the message to that group thread.

- **F4. Send an MLS Squad group message.**
  - **Trigger:** A2 sends a message in a Squad group conversation in OpenClaw.
  - **Steps:**
    1. The plugin calls `agent.send_group_message` with the Squad wire id and content.
    2. The daemon publishes the MLS group message.

## Requirements

### Channel identity and registration

- R1. The plugin must be installable as an OpenClaw channel plugin using the `openclaw/plugin-sdk/channel-outbound` adapter surface.
- R2. The plugin must allow exactly one OpenClaw channel account to map to exactly one Pacto bot identity.
- R3. The plugin must register with the Pacto daemon via `handler.register`, requesting the event types and capabilities required for the configured bot (`dm_received`, `mls_group_message_received`, `ReadMessages`, `SendMessages`, `SendGroupMessages`, `ReceiveGroupMessages`).
- R4. The plugin must store and reuse the server-generated `handler_id` and `reconnect_token` to reconnect after a restart.
- R5. The plugin must display the bot identity's name and picture from the daemon-managed `kind:0` profile.

### Direct messages

- R6. The plugin must support receiving plain-text DMs and presenting them as OpenClaw conversations keyed by the sender npub.
- R7. The plugin must support sending plain-text DMs to a recipient npub via `agent.send_dm`.
- R8. The plugin should map NIP-17 `reply_to` threading to OpenClaw's reply model where the platform supports it; otherwise it may flatten conversations.

### Group messages

- R9. The plugin must support receiving plain-text MLS group messages for Squads where the bot identity is already a member.
- R10. The plugin must support sending plain-text MLS group messages to those Squads.
- R11. The plugin must not create, invite to, or leave Squads; membership is configured out-of-band via `pacto-bot-admin`.

### Transport and security

- R12. The plugin must use the Unix socket transport when co-located with the daemon and the HTTP transport when configured for a remote daemon.
- R13. For HTTP, the plugin must authenticate requests with the `X-Pacto-Bot-Secret` header and include `X-Pacto-Handler-Id` on mutating calls.
- R14. The plugin must reconnect and re-register after daemon restarts or transport failures, and it must not leak secrets, nsec, bunker URIs, or tokens in logs or error messages.

## Acceptance Examples

- **AE1. First DM from a new Nostr user.** Covers R6.
  - **Given:** the channel account is registered and running.
  - **When:** A4 sends a DM to the bot identity.
  - **Then:** OpenClaw creates a new conversation keyed by A4's npub and displays the message.

- **AE2. Reply to an existing DM thread.** Covers R7.
  - **Given:** an existing conversation with a Nostr user.
  - **When:** A2 sends a text message in that conversation.
  - **Then:** the daemon publishes the DM and OpenClaw shows the sent message.

- **AE3. Group message in an existing Squad.** Covers R9.
  - **Given:** the bot identity is a member of Squad S.
  - **When:** a member posts a message in Squad S.
  - **Then:** OpenClaw shows the message in the corresponding group conversation.

- **AE4. Daemon restart recovery.** Covers R4 and R14.
  - **Given:** the plugin is registered and connected over the Unix socket.
  - **When:** the Pacto daemon restarts.
  - **Then:** the plugin reconnects, re-registers with the stored token, and resumes receiving events.

## Scope Boundaries

### Deferred for later

- Media, attachments, reactions, zaps, long-form notes, and other non-DM Nostr event types.
- Creating or inviting the bot into Squads from OpenClaw.
- Admin operations such as profile updates, key rotation, relay configuration, or bot identity creation.
- Mapping more than one Pacto bot identity to a single OpenClaw channel account.
- Auto-discovery of Nostr contacts beyond the first inbound or outbound message.
- A setup wizard for NIP-46 bunker configuration; the operator must configure the daemon and bot identity first.
- Operator overrides for the bot's display name or picture.

### Outside this product's identity

- The plugin is not a standalone Nostr client; it is an OpenClaw channel.
- It does not expose raw Nostr event publishing or signing to the OpenClaw end user.
- It does not replace `pacto-bot-admin` or the Pacto daemon's lifecycle responsibilities.

## Dependencies / Assumptions

- Pacto daemon `>=0.8.0` running with at least one bot identity configured in `pacto-bot-api.toml`.
- OpenClaw's plugin runtime supports the channel-outbound adapter contract.
- The operator has configured the bot identity and any desired Squad memberships via `pacto-bot-admin`.
- The plugin runs with filesystem access to the Unix socket or network access to the HTTP endpoint.
- The plugin can obtain the bot identity's current `kind:0` profile from the daemon or the configured relays.
- NIP-17/NIP-59 encryption and NIP-46 signing remain the daemon's responsibility; the plugin passes plaintext content to and from the daemon.

## Outstanding Questions

- **Deferred to planning:** Exact mapping of NIP-17 `reply_to` chains to OpenClaw's thread model.
- **Deferred to planning:** Whether to cache conversation metadata or look it up on every event.

## Sources / Research

- OpenClaw channel plugin guide: `https://docs.openclaw.ai/plugins/sdk-channel-plugins`
- OpenClaw channel outbound API: `https://docs.openclaw.ai/plugins/sdk-channel-outbound`
- Pacto daemon overview: `AGENTS.md`, `README.md`, `CONCEPTS.md`, `STRATEGY.md`
- Pacto handler JSON-RPC contract: `schemas/jsonrpc.json`
- Pacto transport implementation: `src/transport/http.rs`, `src/transport/unix.rs`, `src/transport/protocol.rs`
- Pacto handler registry and authorization: `src/handlers.rs`, `src/dispatch.rs`
- Pacto event model and capabilities: `src/events.rs`, `CONCEPTS.md`

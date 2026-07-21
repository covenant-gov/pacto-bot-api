---
date: 2026-07-20
topic: bot-mentions-in-squad-channels
---

# Requirements: Bot @ Mentions in Squad Channels

## Summary

Add bot-directed `@` mentions to Pacto Squad channels so users can address a specific bot and avoid collisions when multiple bots share a channel. The Pacto-app UI's existing structured `mentions` array (from the human @mention feature) is reused; the bot's npub is the canonical target. The daemon parses the encrypted JSON envelope, marks whether the receiving bot was mentioned, and forwards the message body to the bot handler. Bot authors control whether their squad commands require a mention.

## Problem Frame

Squad channels are increasingly shared by multiple bots: a governance snapshot bot, a treasury bot, a helper bot, etc. Today every bot receives every decrypted group message and routes purely on content, so a command like `/help` or `!snapshot` is ambiguous and can trigger multiple bots at once. Users expect normal group-chat behavior: type `@Joke Bot /help` to direct the command to the joke bot, and have the bot know it was addressed. Without a structured mention, bot authors cannot safely use common command names in squads.

## Key Decisions

- **SDK commands are mention-gated by default in squads.** `@bot.command("/help")` only fires on `mls_group_message_received` events where the receiving bot's `is_mentioned` is `true`. Bot authors opt out with `require_mention=False`.
- **Resolved: event field names are `is_mentioned`, `mentioned_bot_ids`, and `mentions`.** These names will be added to `schemas/jsonrpc.json` and the generated Python SDK models.
- **Reuse the structured `mentions` array from the Pacto-app UI.** Bot mentions use the same JSON envelope `{ body, mentions }` already defined for human mentions. A bot mention is stored as `{ npub, alias }`; the daemon matches on `npub`, not on the alias or display name.
- **Daemon parses the envelope for bot targets.** The UI doc's "backend never parses" rule is scoped to human-only mentions. For bot routing, the daemon must inspect the `mentions` array.
- **Hybrid dispatch.** All bots still receive every group message, but the forwarded event includes `is_mentioned` and `mentioned_bot_ids` so each bot can decide whether to act. This preserves full-context bots such as LLM-based helpers.
- **Bot author controls reply format.** The daemon does not auto-prefix replies. The SDK exposes the sender (`author`) and mention metadata; the bot author constructs the outbound message.
- **Fallback for legacy content.** If the decrypted group message is not a valid JSON envelope, the daemon treats the entire content as the message body and assumes no mentions. This keeps existing squad messages working.

## Actors

- **A1. Squad member** — types a message and selects a bot from the mention autocomplete.
- **A2. Bot handler** — receives the forwarded `mls_group_message_received` event and decides whether to respond.
- **A3. Other bots in the squad** — receive the same event but are not the mention target.
- **A4. Bot operator** — configures bot identities in `pacto-bot-api.toml`.

## Requirements

### Wire format and parsing

- R1. The daemon parses the decrypted MLS group message content as a JSON envelope containing `body` (string) and `mentions` (array of objects with `npub` and `alias` fields) when the shape is valid.
- R2. If the decrypted content is not valid JSON or lacks the envelope shape, the daemon treats the entire content as the message body and sets the mention list to empty.
- R3. The daemon extracts the `npub` value from each entry in `mentions` and maps it to configured bot identities.

### Event metadata for handlers

- R4. The daemon forwards the `body` string as the `content` field of the `mls_group_message_received` event.
- R5. The daemon forwards the list of mentioned `npub` values (without aliases) to the bot handler as a new `mentions` field on the event.
- R6. The daemon sets `is_mentioned` to `true` on the event if the receiving bot's npub appears in the mention list, otherwise `false`.
- R7. The daemon sets `mentioned_bot_ids` to the list of `bot_id` values whose npubs appear in the mention list. The list may be empty and may include bots other than the receiver.

### SDK behavior

- R8. The SDK exposes `event.is_mentioned` and `event.mentioned_bot_ids` to bot handlers.
- R9. In squad channels, the SDK's `@bot.command` and `@bot.hears` decorators only fire when `is_mentioned` is `true`. Bot authors opt out with an explicit `require_mention=False` flag.
- R10. The SDK continues to give bot authors full control over the reply text and does not auto-prefix outbound squad messages.

### Operational constraints

- R11. The daemon validates that no two configured bots share the same `display_name` and rejects the config at load time. This prevents ambiguity in the UI autocomplete and keeps alias-to-npub resolution deterministic.
- R12. The new mention metadata is added in a backward-compatible way: existing `dm_received` events are unchanged, and legacy group messages without a JSON envelope are handled by R2.

## Key Flows

### F1. Compose and send a bot mention in a squad

- **Trigger:** A1 focuses the squad composer and types `@`.
- **Actors:** A1.
- **Steps:**
  1. The Pacto-app UI shows the bot in the mention autocomplete, searchable by its `display_name`.
  2. A1 selects the bot. The UI inserts `@Joke Bot` into the composer and binds the bot's npub as a mention target.
  3. A1 sends the message. The UI produces the JSON envelope `{ body: "@Joke Bot /help", mentions: [{ npub: "npub1joke...", alias: "Joke Bot" }] }` and encrypts it inside the MLS group message.
  4. The daemon decrypts the message, parses the envelope, and forwards the event to all registered bot handlers.
- **Outcome:** Each bot handler receives the message body and the mention metadata. No mention metadata is visible to relays.

### F2. Bot receives a message where it was mentioned

- **Trigger:** A2 receives an `mls_group_message_received` event whose `mentions` array contains A2's npub.
- **Actors:** A2.
- **Steps:**
  1. The daemon sets `is_mentioned: true` and `mentioned_bot_ids: ["joke-bot"]` on the event.
  2. A2's handler checks `is_mentioned` and routes the command.
  3. A2 replies with `agent.send_group_message` using `event.chat_id` and a message body that addresses `event.author` as the bot author sees fit.
- **Outcome:** A2 responds only when it was explicitly mentioned.

### F3. Bot receives a message where another bot was mentioned

- **Trigger:** A3 receives the same event but its npub is not in `mentions`.
- **Actors:** A3.
- **Steps:**
  1. The daemon sets `is_mentioned: false` and `mentioned_bot_ids: ["joke-bot"]` on the event.
  2. A3's handler can use `mentioned_bot_ids` for context but ignores the command.
- **Outcome:** A3 sees the full squad context but does not respond to the directed command.

### F4. Bot receives a legacy group message

- **Trigger:** A2 receives an `mls_group_message_received` event whose decrypted content is plain text, not a JSON envelope.
- **Actors:** A2.
- **Steps:**
  1. The daemon places the entire content into the `content` field.
  2. The daemon sets `mentions: []`, `is_mentioned: false`, and `mentioned_bot_ids: []`.
- **Outcome:** Legacy messages are handled identically to messages with no mentions.

## Acceptance Examples

- AE1. **Directed command reaches only the target bot.** Given two bots `joke-bot` and `snapshot-bot` in a squad, when A1 sends `@Joke Bot /help`, then `joke-bot` receives `is_mentioned: true` and `mentioned_bot_ids: ["joke-bot"]`, while `snapshot-bot` receives `is_mentioned: false` and `mentioned_bot_ids: ["joke-bot"]`.
- AE2. **Legacy message falls back to no mentions.** Given a squad message sent before the JSON envelope format existed, when the daemon decrypts it, the bot receives the entire content as `content` with `is_mentioned: false` and `mentions: []`.
- AE3. **Common squad command is gated by mention.** Given `joke-bot` registers a `/help` command that requires a mention, when a user sends bare `/help` in the squad, `joke-bot` does not respond; when the user sends `@Joke Bot /help`, it responds.
- AE4. **Duplicate display names are rejected.** Given `pacto-bot-api.toml` contains two bots with `display_name = "Joke Bot"`, the daemon refuses to start and reports a validation error.
- AE5. **No metadata leakage.** Given A1 mentions `joke-bot` in a squad message, the published Kind 444 event contains only the encrypted payload; the `mentions` array, alias, and target npub are not visible to relays.

## Scope Boundaries

### Deferred for later

- DMs with `@` mentions.
- `@all`, `@here`, or role-based squad mentions.
- Native OS push notifications for bot mentions.
- Message editing that preserves or updates mention lists.
- Server-side indexing, search, or analytics based on mentions.

### Outside this product's identity

- Backend parsing of human mentions for notification routing (the UI feature described in the proof doc keeps that client-side).
- Exposing mention metadata outside the MLS ciphertext.
- Supporting mentions that target users or bots outside the squad.

## Dependencies and Assumptions

- **Dependency:** The Pacto-app UI implements the JSON envelope `{ body, mentions }` for squad messages and includes bot npubs as mention targets.
- **Dependency:** The daemon can decrypt MLS group messages and access the plaintext content.
- **Dependency:** Each bot identity in `pacto-bot-api.toml` has a stable `npub` and a `display_name`.
- **Assumption:** Bot authors prefer the structured `npub` target over raw-text display-name matching because it avoids ambiguity and refresh problems.
- **Assumption:** Display-name collisions are rare enough that rejecting them at config load is acceptable; if this becomes a problem, a later design can introduce handles.

## Outstanding Questions

- **Deferred to planning:** How the `python/src/pacto_bot_sdk/bot.py` routing decorators expose the `require_mention` opt-out (parameter on decorators, constructor flag, or both).
- **Deferred to planning:** Migration strategy for existing squad messages that are not JSON envelopes and for existing bot handlers that do not expect the new fields.

## Resolved During Brainstorm

- **SDK default:** `@bot.command` and `@bot.hears` are mention-gated by default in squads; opt out with `require_mention=False`.
- **Field names:** `is_mentioned` (bool), `mentioned_bot_ids` (list of `bot_id` strings), and `mentions` (list of npub strings).

## Sources and Research

- Proof doc for Pacto-app human @mentions (shared by user): `https://www.proofeditor.ai/d/q0wp33r9?token=1175d238-1a21-47b7-8fe3-8a8fabec4879`
- Inbound MLS group message requirements: `docs/plans/2026-07-08-u12-inbound-mls-snapshot-requirements.md`
- Python SDK command routing: `python/src/pacto_bot_sdk/bot.py`
- JSON-RPC event schema: `schemas/jsonrpc.json`

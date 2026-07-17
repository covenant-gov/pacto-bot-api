---
title: "Building Squad bots with the Python SDK"
description: "High-level and low-level guide to writing Pacto bots that join Squads, send and receive MLS group messages, and respond to commands."
date: 2026-07-08
---

# Building Squad bots with the Python SDK

This guide covers how to build a bot that participates in a Pacto Squad using the [`pacto-bot-sdk`](python/) Python SDK. It is written for both human readers and AI assistants, so it alternates between high-level concepts and concrete implementation details.

## What this guide covers

- What a Squad bot is and how it differs from a DM bot.
- The lifecycle of a Squad bot: KeyPackage, Welcome, send, receive, leave.
- How to register a bot with the right capabilities and event types.
- High-level patterns with the `Bot` class.
- Low-level patterns with the generated `PactoClient`.
- A complete example: a governance snapshot bot that responds to `!snapshot`.
- Current gaps in the SDK and how to work around them.
- Security notes and local testing tips.

## What is a Pacto Squad?

A Pacto Squad is an MLS (Messaging Layer Security) group running over Nostr. Messages are encrypted for the group rather than for one recipient, and the group maintains a shared key schedule that advances as members join, leave, or send messages. The daemon owns the MLS engine and keys; the Python bot only sees decrypted plaintext and calls daemon methods to send messages.

## How Squad bots differ from DM bots

| | DM bot | Squad bot |
|---|---|---|
| **Capability** | `SendMessages` / `ReadMessages` | `SendGroupMessages` / `ReceiveGroupMessages` / `ExitMlsGroup` |
| **Event type** | `dm_received` | `mls_group_message_received` |
| **Send method** | `agent.send_dm` | `agent.send_group_message` |
| **Leave method** | — | `agent.exit_mls_group` |
| **Key material** | Daemon uses Nostr signing backend (can be remote bunker). | Daemon holds local MLS keys in `vector-mls.db` per bot. |
| **Joining** | Nothing; DMs are direct. | Bot must publish a KeyPackage and accept a Welcome. |
| **Recipients** | One user. | The whole Squad. |

The bot does not manage MLS keys. The daemon handles KeyPackage publication, Welcome processing, encryption, and decryption. The bot only decides what to send and what to do with received plaintext.

## Capabilities and event types

A Squad bot asks for these capabilities when it registers:

- `SendGroupMessages` — lets the bot post to the Squad.
- `ReceiveGroupMessages` — lets the bot receive decrypted Squad messages.
- `ExitMlsGroup` — lets the bot leave a Squad by publishing a self-removal MLS proposal.

The event type the bot subscribes to is:

- `mls_group_message_received` — a decrypted message was posted to a Squad the bot is in.

These strings are generated automatically from `schemas/jsonrpc.json` and are available in the SDK as typed models and high-level helpers.

## Squad lifecycle from a bot's perspective

```
┌─────────────────┐     ┌────────────────────┐     ┌─────────────────┐
│  Pacto app      │────▶│  Bot publishes     │────▶│  Admin invites  │
│  creates Squad  │     │  KeyPackage (443)  │     │  bot via Welcome│
└─────────────────┘     └────────────────────┘     └─────────────────┘
                                                               │
                                                               ▼
┌─────────────────┐     ┌────────────────────┐     ┌─────────────────┐
│  Bot sends      │◀────│  Daemon decrypts   │◀────│  Member sends   │
│  group message  │     │  group message     │     │  group message  │
└─────────────────┘     └────────────────────┘     └─────────────────┘
```

1. **Squad creation:** A human creates a Squad in the Pacto app. This deploys the on-chain NavePirata contracts and gives the Squad a unique ID.
2. **KeyPackage:** The bot calls `agent.publish_key_package` so the daemon publishes a kind:443 KeyPackage event to the configured relays.
3. **Welcome:** A Squad admin invites the bot. The daemon receives the Welcome inside a kind:1059 GiftWrap, processes it, and persists the group state in `vector-mls.db`.
4. **Send:** The bot calls `agent.send_group_message(bot_id, group_id, content)` (or `bot.send_group_message(group_id, content)`) to post an encrypted message.
5. **Receive:** The daemon subscribes to kind:445 group messages, decrypts them, and delivers plaintext to the bot as `agent.event` notifications.
6. **Leave:** The bot calls `agent.exit_mls_group(bot_id, group_id)` (or `bot.exit_squad(group_id)`) to publish a kind:445 self-removal MLS proposal. An admin must still commit the removal inside the MLS group before the bot is fully removed.

The bot never sees the KeyPackage or Welcome bytes directly. The daemon owns them.

## Prerequisites

- A running `pacto-bot-api` daemon with the bot configured in `pacto-bot-api.toml`.
- The bot must have the `SendGroupMessages` capability enabled in the daemon config.
- The daemon must be able to reach the Nostr relays used by the Squad.
- If the bot reads on-chain data (like the governance snapshot bot), it needs an RPC endpoint.

## Creating a Squad and inviting a bot

The high-level flow is:

1. **Create the Squad in Pacto.** This is a human action in the Pacto app. It deploys the NavePirata contracts and produces a `topHatId` and a Squad address book.
2. **Create the bot identity with `pacto-bot-admin`.**
   ```bash
   pacto-bot-admin new snapshot-bot --backend nsec --relays ws://localhost:7000
   ```
3. **Add the bot to `pacto-bot-api.toml`.** Ensure the bot config includes the Squad relays and has the right capabilities.
4. **Start the bot process.** The bot should call `agent.publish_key_package` on startup. This publishes the KeyPackage to the relays.
5. **Invite the bot in Pacto.** A Squad admin uses the Pacto app to send an invitation. The daemon receives the Welcome, processes it, and stores the group state. The bot is now a member.
6. **Send and receive.** The bot can now post to the Squad and receive messages.

If the bot is restarted, it does not need to be re-invited as long as `vector-mls.db` is intact. It does need to re-publish its KeyPackage if the old one expired or was replaced.

## High-level `Bot` API for Squads

### Registering with Squad capabilities

```python
from pacto_bot_sdk import Bot

bot = Bot(
    bot_id="snapshot-bot",
    event_types=["mls_group_message_received"],
    capabilities=["SendGroupMessages", "ReceiveGroupMessages", "ExitMlsGroup"],
)
```

### Publishing a KeyPackage on startup

The high-level `Bot` class does not yet include a helper for publishing a KeyPackage, so use the generated client directly:

```python
async def publish_key_package(bot):
    event_id = await bot.client.agent_publish_key_package(bot_id=bot.bot_id)
    bot.log(f"published KeyPackage: {event_id}")

# Run once after connecting, before the dispatch loop.
```

The daemon will publish the KeyPackage to the relays and wait for an admin to send a Welcome.

### Handling group messages

Group messages are dispatched the same way as DM commands. The `content` is the decrypted plaintext. The event object includes metadata about the message.

```python
@bot.command("!snapshot")
async def snapshot(event, bot):
    # event.content is the decrypted plaintext, e.g. "!snapshot"
    # event.group_id is the Squad wire id
    # event.author is the sender's pubkey
    snapshot_text = await build_snapshot()
    await bot.send_group_message(group_id=event.group_id, content=snapshot_text)
    return {"event_id": event.event_id, "action": "ack"}
```

For commands that should only work in a Squad, the `event.group_id` tells you which Squad the message came from. The daemon already verified membership before delivery.

### Sending group messages

Use the high-level helper to post to a Squad:

```python
@bot.command("!snapshot")
async def snapshot(event, bot):
    snapshot_text = await build_snapshot()
    event_id = await bot.send_group_message(
        group_id=event.group_id,
        content=snapshot_text,
    )
    bot.log(f"posted group message: {event_id}")
    return {"event_id": event.event_id, "action": "ack"}
```

For advanced cases, call the generated client directly:

```python
async def send_group_message(bot, group_id: str, content: str) -> str:
    event_id = await bot.client.agent_send_group_message(
        bot_id=bot.bot_id,
        group_id=group_id,
        content=content,
    )
    bot.log(f"posted group message: {event_id}")
    return event_id
```

### Leaving a Squad

A bot that has the `ExitMlsGroup` capability can leave a Squad by publishing a self-removal MLS proposal:

```python
@bot.command("!leave")
async def leave(event, bot):
    group_id = event.group_id
    if not group_id:
        return bot.reply(event, "This command only works inside a Squad.")

    event_id = await bot.exit_squad(group_id=group_id)
    bot.log(f"published leave proposal: {event_id}")
    return bot.reply(
        event,
        "Leave proposal published. An admin must commit the removal before I am fully removed.",
    )
```

The helper returns the hex event id of the published kind:445 evolution event. The bot will stop receiving new group messages, but the daemon remains a member of the MLS group until an admin commits the removal proposal.

### Rate-limiting responses

Rate limiting should be per Squad, not per user. A common pattern is one request per Squad per minute. If the limit is exceeded, post a group message explaining the limit instead of doing the work again.

```python
from datetime import datetime, timedelta
from collections import defaultdict

last_snapshot: dict[str, datetime] = defaultdict(lambda: datetime.min)

async def snapshot(event, bot):
    now = datetime.utcnow()
    group_id = event.group_id
    if now - last_snapshot[group_id] < timedelta(minutes=1):
        await bot.send_group_message(
            group_id=group_id,
            content="One snapshot per minute per Squad, please.",
        )
        return {"event_id": event.event_id, "action": "ack"}

    last_snapshot[group_id] = now
    snapshot_text = await build_snapshot()
    await bot.send_group_message(group_id=group_id, content=snapshot_text)
    return {"event_id": event.event_id, "action": "ack"}
```

This keeps the bot honest about load and prevents the same Squad from accidentally spamming itself.

### DM command membership verification

A user can also send `!snapshot` via DM. In that case the bot must verify the user is a member of the Squad they name. Use the high-level helper or the generated client directly:

```python
@bot.command("!snapshot")
async def snapshot_dm(event, bot):
    # Parse the DM to extract the Squad ID, e.g. "!snapshot <group-id>"
    user_specified_group_id = parse_squad_id(event.content)

    is_member = await bot.is_squad_member(
        group_id=user_specified_group_id,
        member_pubkey=event.author,  # the DM sender's pubkey
    )

    if not is_member:
        return bot.reply(event, "You are not a member of that Squad.")

    snapshot_text = await build_snapshot()
    await bot.send_group_message(
        group_id=user_specified_group_id,
        content=snapshot_text,
    )
    return {"event_id": event.event_id, "action": "ack"}
```

The user must include the Squad ID in the DM command because the DM itself is not tied to a Squad. The daemon performs the actual membership check against the MLS group state it holds.

## Low-level `PactoClient` API

For advanced bots, use the generated client directly. The relevant methods are:

### `handler.register`

```python
from pacto_bot_sdk import PactoClient
from pacto_bot_sdk.transports import UnixTransport

client = PactoClient(UnixTransport("/tmp/pacto-bot-api.sock"))
await client.connect()

result = await client.handler_register(
    bot_ids=["snapshot-bot"],
    event_types=["mls_group_message_received"],
    capabilities=["SendGroupMessages", "ReceiveGroupMessages", "ExitMlsGroup"],
)
handler_id = result.handler_id
```

### `agent.publish_key_package`

```python
event_id = await client.agent_publish_key_package(bot_id="snapshot-bot")
```

This is the first step the bot must take after connecting. The daemon returns the kind:443 event id.

### `agent.send_group_message`

```python
from pacto_bot_sdk._generated.models import AgentSendGroupMessageParams

event_id = await client.agent_send_group_message(
    bot_id="snapshot-bot",
    group_id="<hex-group-id>",
    content="Hello, Squad!",
)
```

### `agent.exit_mls_group`

```python
from pacto_bot_sdk._generated.models import AgentExitMlsGroupParams

result = await client.agent_exit_mls_group(
    bot_id="snapshot-bot",
    group_id="<hex-group-id>",
)
leave_event_id = result.event_id
```

This publishes a kind:445 evolution event containing a self-removal MLS proposal. The bot stops receiving new group messages, but an admin must still commit the removal inside the MLS group before the bot is fully removed from the roster.

### `agent.event` shape for group messages

The daemon delivers an `AgentEventParams` notification with:

- `type`: `"mls_group_message_received"`
- `bot_id`: the bot that received the message.
- `group_id`: the Squad wire id.
- `content`: decrypted plaintext.
- `author`: the sender's Nostr pubkey.
- `event_id`: the kind:445 wrapper event id.
- `timestamp`: the wrapper event's created-at timestamp.
- `chat_id`: for group messages, this mirrors `group_id`.
- `rumor_id`: the inner MLS message id, when available.

Use `event_id` for deduplication. Use `group_id` to scope rate limits and to route sends.

### Membership verification

```python
from pacto_bot_sdk._generated.models import AgentIsSquadMemberParams

result = await client.agent_is_squad_member(
    bot_id="snapshot-bot",
    group_id="<hex-group-id>",
    member_pubkey="<npub-or-hex>",
)
is_member = result.is_member
```

This lets the bot verify DM-triggered commands without parsing the MLS group state itself.

## Example: a governance snapshot bot

```python
from datetime import datetime, timedelta
from collections import defaultdict
from pacto_bot_sdk import Bot

bot = Bot(
    bot_id="snapshot-bot",
    event_types=["mls_group_message_received", "dm_received"],
    capabilities=["SendGroupMessages", "ReceiveGroupMessages", "ExitMlsGroup"],
)

last_snapshot: dict[str, datetime] = defaultdict(lambda: datetime.min)

async def build_snapshot() -> str:
    # Read on-chain state and format a Markdown snapshot.
    return "# Governance snapshot\n\n..."


def parse_squad_id(content: str) -> str:
    # Expects "!snapshot <group-id>" or just "!snapshot" when inside a Squad.
    parts = content.strip().split(None, 1)
    return parts[1] if len(parts) > 1 else ""


async def maybe_snapshot(group_id: str, author: str) -> str:
    now = datetime.utcnow()
    if now - last_snapshot[group_id] < timedelta(minutes=1):
        return "One snapshot per minute per Squad, please."
    last_snapshot[group_id] = now
    return await build_snapshot()


@bot.command("!snapshot")
async def snapshot(event, bot):
    group_id = event.group_id
    if not group_id:
        return bot.reply(event, "This command only works inside a Squad.")
    content = await maybe_snapshot(group_id, event.author)
    await bot.send_group_message(group_id=group_id, content=content)
    return {"event_id": event.event_id, "action": "ack"}


@bot.command("!leave")
async def leave(event, bot):
    group_id = event.group_id
    if not group_id:
        return bot.reply(event, "This command only works inside a Squad.")
    event_id = await bot.exit_squad(group_id=group_id)
    bot.log(f"published leave proposal: {event_id}")
    return bot.reply(
        event,
        "Leave proposal published. An admin must commit the removal before I am fully removed.",
    )


@bot.dm
async def snapshot_dm(event, bot):
    user_specified_group_id = parse_squad_id(event.content)
    if not user_specified_group_id:
        return bot.reply(event, "Usage: !snapshot <group-id>")

    is_member = await bot.is_squad_member(
        group_id=user_specified_group_id,
        member_pubkey=event.author,
    )
    if not is_member:
        return bot.reply(event, "You are not a member of that Squad.")

    content = await maybe_snapshot(user_specified_group_id, event.author)
    await bot.send_group_message(group_id=user_specified_group_id, content=content)
    return {"event_id": event.event_id, "action": "ack"}


if __name__ == "__main__":
    bot.run()
```

The example uses the high-level helpers for sending, membership verification, and leaving. It also demonstrates a DM-triggered path that checks membership before posting to a Squad.

## Gaps and workarounds

| Gap | Workaround |
|---|---|
| The high-level `Bot` class has no helper for publishing a KeyPackage. | Use `bot.client.agent_publish_key_package` directly after the bot connects. |
| There are no Squad examples in `python/examples/`. | Use the high-level helpers in this guide as a starting point. |

After the daemon schema changes, run `cargo xtask codegen` to regenerate the Python SDK. The generated models and docstrings will then include the new methods and event types.

## Security notes

- The daemon holds the MLS keys in `vector-mls.db`. The Python bot never sees them.
- Do not log the decrypted content of private Squad messages unless your bot needs to for debugging, and never log secrets or key material.
- The bot should verify DM-triggered commands by asking the daemon, not by trusting the user.
- Rate limiting per Squad prevents a single Squad from accidentally overwhelming the bot and the relays.
- For production, run the daemon inside a TEE (see `docs/tee-private-agent-architecture.md`). This is the only way to protect `vector-mls.db` from a compromised host.

## Testing locally

1. Run a local Nostr relay (e.g., `strfry` or `nostream`) and an optional EVM node like anvil.
2. Start the daemon with a test bot identity.
3. Run a minimal Squad bot:
   ```bash
   python my_squad_bot.py --socket /tmp/pacto-bot-api.sock
   ```
4. In the Pacto app, create a Squad and invite the bot. The daemon should process the Welcome automatically.
5. Send a message in the Squad and verify the bot receives it.

For unit tests, use the `PactoClient` directly with a mock transport or a test daemon fixture. Avoid real relays in CI.

## Troubleshooting

- **Bot never receives group messages:** Check that the bot registered with `ReceiveGroupMessages` and `event_types=["mls_group_message_received"]`. Verify the bot accepted the Welcome (`pacto-bot-admin status` should show the bot as healthy).
- **Bot cannot send:** Check that the bot has the `SendGroupMessages` capability and that the Welcome was accepted (the bot must be a group member before it can send).
- **KeyPackage not found:** Make sure the daemon published the KeyPackage after the bot started. The bot must call `agent.publish_key_package` before an admin can invite it.
- **Rate-limit message is posted too often:** Ensure the rate-limit window is tracked per `group_id`, not per `event_id` or per user.
- **DM command fails membership check:** Verify the DM sender is using the correct Squad ID and that the bot is a member of that Squad. The daemon performs the check against the MLS group state, not against on-chain Hats.

## References

- `python/README.md` — general Python SDK usage.
- `python/examples/greeting_bot.py` — DM bot example that shows transport and registration.
- `schemas/jsonrpc.json` — canonical JSON-RPC contract for the daemon.
- `docs/tee-private-agent-architecture.md` — long-term security model for Squad key custody.
- `docs/brainstorms/2026-07-08-u12-inbound-mls-snapshot-requirements.md` — requirements for the daemon's inbound MLS support.
- `pacto-app` — reference Pacto client implementation that uses the same `mdk-core` MLS library.

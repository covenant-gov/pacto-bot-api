---
title: SDK Usability Recommendations — Bosun Phase 2 Review
type: recommendation
status: proposed
date: 2026-07-08
origin: /tmp/compound-engineering/ce-code-review/20260708-150009-62d63dd7/report.md
---

# SDK Usability Recommendations — Bosun Phase 2 Review

## Summary

This document collects SDK-level improvements that would have made the Bosun Phase 2 implementation (`!snapshot` inbound triggers, rate-limit replies, and DM membership checks) simpler, safer, and less coupled to internal SDK hooks. The recommendations are derived from the code review of PR #5 in `pacto-governance-bots` and from inspecting the current `pacto_bot_sdk` source.

The overarching theme is that the high-level `Bot` class already provides a clean decorator API for slash commands (`@bot.command`), default handlers (`@bot.default`), status notifications (`@bot.status`), and rate-limit notifications (`@bot.rate_limited`). The missing pieces are the same kind of first-class registration for **plain-text group/DM events** and a few **cross-cutting helpers** (identity, timeouts, throttling, concurrency) that every bot author will need.

## Problem Statements

1. **Plain-text commands require overriding `_handle_event`.** Bosun needs to respond to `!snapshot` in group messages and DMs. The SDK only parses slash commands (`/snapshot`) automatically. To intercept non-slash text, the bot must subclass `Bot` and override `_handle_event`, which loses the built-in `handler_response` acknowledgement and duplicates routing logic.
2. **Event acknowledgement is easy to forget.** The base `_handle_event` sends `handler_response` automatically for decorated commands, but once a bot overrides the method, every early-return path must remember to acknowledge the event. Missing this causes daemon retries, duplicate triggers, and handler degradation.
3. **The bot's own public key is not exposed.** Bosun adds a guard against self-authored messages, but `own_pubkey` is not populated by the SDK. The guard is dead code unless the bot author manually derives the key.
4. **Rate-limit handling is not available until the upstream PR lands.** The SDK source already has `@bot.rate_limited`, but the version installed by Bosun at review time did not dispatch `AgentRateLimitedParams`. Clear versioning and release notes would reduce this friction.
5. **No built-in timeouts, throttling, or concurrency control.** The generated client awaits futures indefinitely. A slow RPC stalls the entire dispatch loop. There is no SDK helper to throttle repeated triggers or serialize overlapping calls to `snapshot()`.
6. **Squad ID and pubkey validation is left to each bot.** `agent_is_squad_member` and `agent_send_group_message` accept raw strings; a malformed ID produces confusing daemon errors.
7. **Unknown notification types are silently dropped.** The SDK's `_dispatch_loop` only dispatches event/status/rate-limited. Future notification types will be swallowed unless the bot overrides the loop.

## Recommendations

### R1. Add event-type decorators for non-slash inbound events

**Priority:** P1
**Owner:** SDK maintainer

Add decorators that register handlers for specific `agent.event` subtypes without requiring subclasses to override `_handle_event`.

Suggested API:

```python
@bot.event("mls_group_message_received")
async def on_group_message(event: AgentEventParams, bot: Bot) -> dict[str, Any] | None:
    ...

@bot.dm
async def on_dm(event: AgentEventParams, bot: Bot) -> dict[str, Any] | None:
    ...
```

Behavior:

- The SDK's `_handle_event` checks `event.type` and routes to the registered handler.
- If the handler returns `None`, the SDK sends `handler_response(action="ignore", event_id=event.event_id)` automatically.
- If the handler returns a dict, the SDK validates the `event_id`/`action` contract and sends `handler_response`.
- A fallback `@bot.default` is still used when no event-type handler matches.

This removes the need for Bosun to override `_handle_event` entirely.

### R2. Add a plain-text command/hears decorator

**Priority:** P1
**Owner:** SDK maintainer

Provide a decorator for literal text commands that is symmetric with `@bot.command` for slash commands.

Suggested API:

```python
@bot.hears("!snapshot")
async def on_snapshot(event: AgentEventParams, bot: Bot) -> dict[str, Any] | None:
    await bot.send_group_message(group_id=event.chat_id, content="...")
    return {"event_id": event.event_id, "action": "ignore"}
```

Behavior:

- Match the trimmed first token exactly (not a substring prefix).
- Optionally support a regex variant: `@bot.hears(re.compile(r"!snapshot(?:\s+(\S+))?"))`.
- Return the same response contract as `@bot.command`.

This would let Bosun register `!snapshot` the same way it registers `/snapshot`, and the parser inconsistency between group messages and DMs would disappear.

### R3. Auto-acknowledge events and provide helper responses

**Priority:** P1
**Owner:** SDK maintainer

Make it impossible for a bot author to forget the daemon acknowledgement.

Suggested API:

```python
# Inside handler helpers
bot.ignore(event)           # -> {"event_id": event.event_id, "action": "ignore"}
bot.reply(event, "text")   # -> {"event_id": event.event_id, "action": "reply", "content": "text"}
```

Behavior:

- For decorated handlers, the SDK always sends `handler_response` after the handler returns, even if it raises.
- For event-type handlers, `None` is treated as `action="ignore"`.
- Helpers make the response contract explicit and easy to unit test.

Bosun's current bug (missing `handler_response` for `mls_group_message_received` and DM `!snapshot`) would be prevented by this change.

### R4. Expose the bot's own public key

**Priority:** P2
**Owner:** SDK maintainer

Populate `bot.own_pubkey` from the daemon's registration response or from `agent.version` / a dedicated `agent.get_bot_pubkey` call.

Suggested API:

```python
class Bot:
    @property
    def own_pubkey(self) -> str | None:
        ...
```

If the daemon cannot provide the key, document the limitation and remove the partial guard from the SDK examples. Bosun's self-message filter would then be useful instead of dead code.

### R5. Add per-request timeouts to the generated client

**Priority:** P2
**Owner:** SDK maintainer

The generated client awaits futures indefinitely. Add an optional `timeout` parameter to client methods and/or a global `PactoClient(timeout=...)` default.

Suggested API:

```python
await bot.client.agent_is_squad_member(
    bot_id=bot.bot_id,
    group_id=group_id,
    member_pubkey=member_pubkey,
    timeout=10.0,
)
```

Behavior:

- Convert `TimeoutError` into a well-typed `PactoClientError` subclass or raise `asyncio.TimeoutError` consistently.
- Document the default timeout value and how to override it.

This would let Bosun remove the head-of-line blocking risk without wrapping every call in `asyncio.wait_for`.

### R6. Provide a built-in throttling/rate-limit helper

**Priority:** P2
**Owner:** SDK maintainer

Add a decorator or context manager for per-key throttling so bots can enforce local defense-in-depth without reimplementing a timestamp map.

Suggested API:

```python
@bot.throttle(key=lambda event: event.chat_id, window_seconds=60)
async def on_snapshot(event: AgentEventParams, bot: Bot) -> None:
    await snapshot(bot, group_id=event.chat_id)
```

Behavior:

- Track the last invocation time per key in memory.
- If the key is throttled, return `None` (which becomes `action="ignore"`) or call an optional `on_throttled` callback.
- Reset on bot restart (in-memory only).

This would address Bosun's DM-triggered `!snapshot` bypass of the Squad-channel rate limit and the concurrent-cadence/dispatch double-post risk.

### R7. Provide a concurrency lock helper

**Priority:** P2
**Owner:** SDK maintainer

Add a simple way to serialize handler invocations so that overlapping `snapshot()` calls do not double-post.

Suggested API:

```python
@bot.lock(name="snapshot")
async def on_snapshot(event: AgentEventParams, bot: Bot) -> None:
    await snapshot(bot, group_id=event.chat_id)
```

Behavior:

- Use an `asyncio.Lock` stored on the `Bot` instance.
- If the lock is held, queue or skip the invocation based on a configurable policy.

Bosun would use this to prevent the daily cadence and an inbound `!snapshot` from posting two snapshots simultaneously.

### R8. Add a high-level `send_group_message` helper on `Bot`

**Priority:** P3
**Owner:** SDK maintainer

The SDK already has `send_dm`. Add a symmetric `send_group_message` helper.

Suggested API:

```python
await bot.send_group_message(group_id=group_id, content=content)
```

This reduces boilerplate and makes `Bot` a complete facade for the generated client.

### R9. Add an `is_squad_member` helper on `Bot`

**Priority:** P3
**Owner:** SDK maintainer

Provide a bot-scoped helper that fills in `bot_id` automatically.

Suggested API:

```python
is_member = await bot.is_squad_member(group_id=group_id, member_pubkey=member_pubkey)
```

This is a thin convenience wrapper around `bot.client.agent_is_squad_member`, consistent with the existing `send_dm` helper.

### R10. Provide input validation helpers

**Priority:** P3
**Owner:** SDK maintainer

Add small validators for common wire types (Squad IDs, pubkeys, event IDs) so bots can fail fast with clear errors.

Suggested API:

```python
from pacto_bot_sdk import validate

validate.squad_id(group_id)  # raises ValueError or returns normalized id
validate.pubkey(member_pubkey)
```

Bosun could use these before calling `agent_is_squad_member` and `agent_send_group_message`, reducing noisy logs and daemon errors.

### R11. Log unknown notification types by default

**Priority:** P3
**Owner:** SDK maintainer

In `_dispatch_loop`, add an `else` branch that logs the unexpected notification type at warning level.

```python
else:
    self._logger.warn(f"unknown notification type: {type(notification).__name__}")
```

This makes future SDK additions observable and reduces the risk of silently breaking bot behavior when new notifications are added.

### R12. Document the handler response contract clearly

**Priority:** P3
**Owner:** SDK maintainer

The `Bot` class examples show handlers returning dicts, but the contract (`event_id`, `action`, optional `content`) is not prominent. Add a dedicated section in the SDK README with examples for `ignore`, `reply`, and error handling.

## Implementation Notes

- These recommendations are additive; none require breaking changes to existing `@bot.command` or `@bot.default` behavior.
- R1 and R2 are the highest leverage: they would let Bosun remove the `_handle_event` and `_dispatch_loop` overrides entirely and return to the decorator API that the project conventions already prefer.
- R3, R4, R5, R6, and R7 address operational safety concerns that repeatedly appeared across multiple reviewer personas (correctness, reliability, security, adversarial).
- R8, R9, R10, R11, and R12 are quality-of-life improvements that reduce boilerplate and improve observability.

## Appendix: Sketch of a simplified Bosun with the proposed SDK

```python
from pacto_bot_sdk import AgentEventParams, Bot
from bosun.bosun import snapshot

bot = Bot(
    bot_id=settings.bot_id,
    capabilities=["SendGroupMessages", "ReceiveGroupMessages"],
    event_types=["dm_received", "mls_group_message_received"],
)

@bot.hears("!snapshot")
@bot.throttle(key=lambda e: e.chat_id, window_seconds=60)
@bot.lock(name="snapshot")
async def on_snapshot(event: AgentEventParams, bot: Bot) -> None:
    await snapshot(bot, group_id=event.chat_id)

@bot.command("/snapshot")
@bot.lock(name="snapshot")
async def on_slash_snapshot(event: AgentEventParams, bot: Bot) -> dict[str, Any]:
    await snapshot(bot)
    return bot.reply(event, "Snapshot posted to the squad channel.")

@bot.rate_limited
async def on_rate_limited(params: AgentRateLimitedParams, bot: Bot) -> None:
    window = params.window_seconds or 60
    await bot.send_group_message(
        group_id=params.group_id,
        content=f"> Rate limit: one snapshot per minute per Squad. Try again in ~{window} seconds.",
    )
```

With these SDK changes, Bosun would no longer need to subclass `Bot`, override `_handle_event`, or override `_dispatch_loop`. The resulting code would be smaller, more idiomatic, and less fragile against future SDK updates.

## Acceptance Criteria

- AC1. A bot can register a plain-text command handler without overriding `_handle_event`.
- AC2. A bot can register a handler for a specific `agent.event` type without overriding `_handle_event`.
- AC3. The SDK automatically sends `handler_response` for all registered handlers when they return `None` or a valid response dict.
- AC4. `Bot` exposes `own_pubkey` populated from the daemon or registration response.
- AC5. Generated client methods accept an optional `timeout` parameter.
- AC6. The SDK provides a throttling decorator and a concurrency-lock decorator.
- AC7. The SDK logs unknown notification types at warning level instead of silently dropping them.
- AC8. SDK documentation includes a clear handler-response contract and examples for `ignore` and `reply`.

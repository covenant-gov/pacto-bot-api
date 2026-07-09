# SDK Usability Improvements Summary

## Overview

This release introduces significant usability improvements to the Python SDK based on recommendations from the Bosun Phase 2 review. The changes focus on making the SDK more intuitive and powerful while reducing the need for bot authors to subclass or override internal methods.

## Key Features

### 1. Event-Type Routing Decorators
- `@bot.event(type)` - Register handlers for specific `agent.event` notification types
- `@bot.dm` - Shorthand for `@bot.event("dm_received")`
- Eliminates need to override `_handle_event` for non-slash command handling

### 2. Plain-Text Command Decorator
- `@bot.hears(token)` - Match plain-text commands by first token
- Symmetric API with `@bot.command` for slash commands
- Handles group messages and DMs without subclassing

### 3. Guaranteed Acknowledgement
- `auto_acknowledge=True` by default (breaking change mitigation available)
- `bot.ignore(event)` and `bot.reply(event, content)` helper methods
- Eliminates forgotten daemon acknowledgements that cause retries

### 4. Bot Identity Helpers
- `bot.own_pubkey` property populated from daemon registration
- `bot.send_group_message()` - High-level group message helper
- `bot.is_squad_member()` - Check membership with automatic bot_id

### 5. Timeout Support
- Per-request timeouts with 30-second default
- Configurable at client construction and per-method call
- Clear `PactoClientError` on timeout

### 6. Throttling & Concurrency Control
- `@bot.throttle(key, window_seconds)` - Rate limit by key function
- `@bot.lock(name, on_conflict, max_waiters)` - Serialize handler execution
- In-memory state with automatic cleanup

### 7. Input Validation
- `pacto_bot_sdk.validate` module with `squad_id`, `pubkey`, `event_id` validators
- Defensive bounds checking for common wire types

### 8. Improved Observability
- Unknown notification types logged at warning level (once per type)
- Clear handler response contract documentation

## Breaking Changes

### Default Behavior Change
- `Bot(..., auto_acknowledge=True)` is now the default
- Handlers returning `None` now automatically send `handler_response(action="ignore")`
- Existing bots can set `auto_acknowledge=False` for transition period

### Migration Path
```python
# Before (manual acknowledgement)
@bot.command("/hello")
async def hello(event, bot):
    # Had to remember to send handler_response
    return {"event_id": event.event_id, "action": "ignore"}

# After (automatic acknowledgement)  
@bot.command("/hello")
async def hello(event, bot):
    # None return automatically becomes ignore response
    return None

# Or use explicit helpers
@bot.command("/hello")
async def hello(event, bot):
    return bot.ignore(event)
    
# Or reply directly
@bot.command("/hello")
async def hello(event, bot):
    return bot.reply(event, "Hello!")
```

## Usage Examples

### Event-Type Handler
```python
@bot.event("mls_group_message_received")
async def on_group_message(event, bot):
    # Handle group messages without subclassing
    await bot.send_group_message(event.chat_id, "Acknowledged!")
    return bot.ignore(event)
```

### Plain-Text Command
```python
@bot.hears("!snapshot")
@bot.throttle(key=lambda e: e.chat_id, window_seconds=60)
async def snapshot_command(event, bot):
    # Handle !snapshot in group messages or DMs
    # Throttled to once per minute per chat
    # ...
    return bot.reply(event, "Snapshot created!")
```

### Combined Decorators
```python
@bot.hears("!status")
@bot.throttle(key=lambda e: e.author, window_seconds=30)
@bot.lock(name="status", on_conflict="skip")
async def status_command(event, bot):
    # Throttle by user, skip if already running
    status = get_system_status()
    return bot.reply(event, f"Status: {status}")
```

## Impact

These changes address all recommendations from the Bosun Phase 2 review:

1. ✅ R1: Event-type decorators for non-slash inbound events
2. ✅ R2: Plain-text command/hears decorator 
3. ✅ R3: Auto-acknowledge events and provide helper responses
4. ✅ R4: Expose the bot's own public key
5. ✅ R5: Add per-request timeouts to the generated client
6. ✅ R6: Provide a built-in throttling/rate-limit helper
7. ✅ R7: Provide a concurrency lock helper
8. ✅ R8: Add high-level `send_group_message` helper
9. ✅ R9: Add `is_squad_member` helper
10. ✅ R10: Provide input validation helpers
11. ✅ R11: Log unknown notification types by default
12. ✅ R12: Document the handler response contract clearly

The SDK is now significantly more approachable for new bot authors while providing powerful primitives for complex bots.
# pacto-python-bot

Use this skill when the user asks to write, scaffold, or modify a Python bot for the `pacto-bot-api` daemon using the generated SDK.

## Trigger phrases

- "create a Python bot"
- "write a pacto bot"
- "add a bot example"
- "handler in Python"
- "pacto_bot_api SDK"

## Canonical references

Always read these first; they are the single source of truth:

- `python/README.md` — installation, quickstart, daemon setup, transport resolution, canonical bot loop.
- `python/examples/greeting_bot.py` — minimal reply bot using `@bot.command` and `@bot.default`.
- `python/examples/joke_bot.py` — `defer` action + proactive `bot.send_dm`.
- `examples/echo_bot.manifest.json` and `python/examples/greeting_bot.manifest.json` — contract harness manifest templates.
- `examples/test_examples_contract.py` — how new examples are validated.

## Bot workflow

Every Python bot follows the same loop:

1. **Register** — `Bot` calls `handler.register` with `bot_ids`, `event_types`, and `capabilities`.
2. **Receive** — daemon sends `agent.event` notifications of type `dm_received`.
3. **Dispatch** — `@bot.command("/name")` or `@bot.default` receives the parsed command.
4. **Return** — handler returns `{"event_id": ..., "action": ..., "content": ...}`; `Bot` sends `handler.response`.

Valid actions: `ack`, `reply`, `defer`, `ignore`.

## Minimum viable bot

```python
#!/usr/bin/env python3
"""One-line description of what this bot does.

Capabilities required:
    - ReadMessages
    - SendMessages

Usage:
    python my_bot.py --socket /tmp/pacto.sock
"""

from __future__ import annotations

from pacto_bot_api import Bot

bot = Bot(bot_id="my-bot")


@bot.command("/hello")
async def hello(event, bot):
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": "Hello!",
    }


@bot.default
async def unknown(event, bot):
    return {"event_id": event.event_id, "action": "ignore"}


if __name__ == "__main__":
    bot.run()
```

## Common patterns

### Command with positional args and flags

```python
@bot.command("/greet")
async def greet(event, bot):
    parsed = event.content  # Bot.command receives the raw event; parse manually if needed.
    # Note: the high-level Bot does not auto-parse args for the handler.
    # Use bot.client or parse event.content inside the handler.
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": "Hello!",
    }
```

### Defer + proactive send_dm

```python
import asyncio

async def _deliver_later(bot, event):
    await asyncio.sleep(0.5)
    await bot.send_dm(recipient=event.author, content="Done!")

@bot.command("/later")
async def later(event, bot):
    asyncio.create_task(_deliver_later(bot, event))
    return {"event_id": event.event_id, "action": "defer"}
```

### Using the low-level client directly

```python
from pacto_bot_api import PactoClient
from pacto_bot_api.transports import UnixTransport

client = PactoClient(UnixTransport("/tmp/pacto.sock"))
await client.connect()
result = await client.handler_register(...)
```

## Adding a new example

If the user wants a new example in `python/examples/`:

1. Create `<name>_bot.py` using the `Bot` decorator API.
2. Add a module docstring with capabilities and usage.
3. Create `<name>_bot.manifest.json` matching the schema in `examples/example-manifest.json`.
4. Run the contract harness:
   ```bash
   cd examples
   python -m pytest test_examples_contract.py -v
   ```
5. Ensure `python/examples/<name>_bot.py` is discovered automatically; `examples/conftest.py` scans both `examples/` and `python/examples/`.

## Daemon setup commands

Use only commands that exist in `pacto-bot-admin`:

```bash
pacto-bot-admin new my-bot --backend nsec --relays ws://localhost:7000
pacto-bot-admin publish-profile my-bot
pacto-bot-admin rotate-http-token --data-dir ~/.local/share/pacto-bot-api
pacto-bot-api --config pacto-bot-api.toml --data-dir ~/.local/share/pacto-bot-api --http-bind 127.0.0.1:8080
```

Do **not** invent commands like `pacto bunker init` or `pacto bot create`; they do not exist yet.

## Verification checklist

- [ ] Bot imports `from pacto_bot_api import Bot`.
- [ ] `Bot(bot_id=...)` is constructed with explicit id.
- [ ] At least one `@bot.command` or `@bot.default` is registered.
- [ ] Handler returns a dict with `event_id` and `action`, or `None`.
- [ ] Example has a manifest if it should be validated by the contract harness.
- [ ] Contract harness passes: `pytest examples/test_examples_contract.py -v`.
- [ ] No invented admin CLI commands appear in docs or docstrings.

## Anti-patterns

- Do not use the hand-written seed `examples/pacto_sdk.py` for new bots; use the generated SDK.
- Do not commit real `nsec` values or secret tokens.
- Do not block the dispatch loop with long synchronous work; use `asyncio.create_task` for deferred actions.

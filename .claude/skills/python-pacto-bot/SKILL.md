# python-pacto-bot

Use this skill **only** when the user explicitly asks to write, scaffold, or modify a Python bot for the `pacto-bot-api` daemon using the generated `pacto_bot_sdk` SDK.

This skill is **not** for generic bots, Discord bots, Slack bots, Telegram bots, or any other bot framework. If the user says "create a bot" without mentioning Pacto or `pacto_bot_sdk`, do **not** load this skill.

## Trigger phrases

- "create a python pacto bot"
- "write a python bot for pacto-bot-api"
- "pacto python bot"
- "pacto_bot_sdk bot"
- "pacto_bot_sdk SDK bot"
- "add a pacto python example bot"
- "pacto python handler"
- "python bot using pacto_bot_sdk"

## Disambiguation

| User says | Load this skill? |
|---|---|
| "create a python pacto bot" | Yes |
| "write a bot for pacto-bot-api" | Yes |
| "pacto_bot_sdk SDK" | Yes |
| "create a bot" | No — too generic |
| "discord bot" / "slack bot" / "telegram bot" | No — wrong framework |
| "rust bot" / "go bot" | No — wrong language |

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

## Scaffold a new Python bot project

For a brand-new bot, start with the admin CLI scaffold generator instead of hand-writing files. This creates an opinionated, runnable project with the handler file, `Dockerfile`, `docker-compose.yml`, systemd unit, `pacto-bot-api.toml`, `README.md`, and pytest files.

```bash
# Create a new bot identity and scaffold a Python handler project
pacto-bot-admin new --scaffold my-bot --backend nsec --relays ws://localhost:7000 --commands hello,help

# Or scaffold a project for an existing identity already in pacto-bot-api.toml
pacto-bot-admin scaffold my-bot --commands hello,help

# Scaffold a bot that calls external HTTP APIs (adds httpx + respx)
pacto-bot-admin scaffold my-bot --commands price --http
```

The generated project is rendered from the `python-llm` template in the external `pacto-bot-templates` repository. Inside the project:

- `bots/my-bot/my_bot.py` — handler file using `from pacto_bot_sdk import Bot`.
- `bots/my-bot/Dockerfile` — container image for the bot.
- `docker-compose.yml` — compose stack for the bot, daemon, and optional bunker.
- `pacto-bot-api.toml` — daemon config with the bot identity (mode `0o600`).
- `README.md` — how to run against a host daemon or the full compose stack.

Only fall back to hand-writing the minimum viable bot below when the user explicitly wants a single-file example or is modifying an existing file that cannot be regenerated.

### Secret handling

- `pacto-bot-admin new` and `pacto-bot-admin new --scaffold` with `--backend nsec` generate an `nsec` value and write it to `pacto-bot-api.toml`.
- **Never paste the generated `nsec` value, config snippet, or any signing material into chat.** The file is created with `0o600` permissions precisely because it contains secrets.
- For local development the `nsec` backend is acceptable; for production use `bunker_local` or `bunker_remote` instead.
- When sharing generated files, redact `nsec`, `uri`, and `secret_token` values first.

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

from pacto_bot_sdk import Bot, parse_command

bot = Bot(bot_id="my-bot")


@bot.command("/hello")
async def hello(event, bot):
    parsed = parse_command(event.content)
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

## Error handling

By default, `Bot` catches unhandled handler exceptions and replies with a
friendly message so users know the bot is alive. Control this with constructor
options:

```python
bot = Bot(
    bot_id="my-bot",
    reply_on_error=False,          # silently ignore handler exceptions
    error_message="Oops, try again.",
)
```

The error message is never sent for non-command events (e.g., malformed input)
and never includes raw exception details.

## Common patterns

### Command with positional args and flags

```python
from pacto_bot_sdk import parse_command

@bot.command("/greet")
async def greet(event, bot):
    parsed = parse_command(event.content)
    name = parsed["args"][0] if parsed and parsed["args"] else "friend"
    return {"event_id": event.event_id, "action": "reply", "content": f"Hello, {name}!"}
```

### HTTP client helper

If the bot calls external APIs, install the optional `http` extra:

```bash
pip install "pacto-bot-api[http]"
```

Then use `httpx` in a handler:

```python
import httpx

@bot.command("/price")
async def price(event, bot):
    async with httpx.AsyncClient() as client:
        resp = await client.get("https://api.example.com/price")
    data = resp.json()
    return {"event_id": event.event_id, "action": "reply", "content": str(data)}
```

When scaffolding with `--http`, the generated project includes `httpx` as a
runtime dependency, `respx` in dev dependencies, and a `tests/test_http.py`
template showing how to mock external calls.

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
from pacto_bot_sdk import PactoClient
from pacto_bot_sdk.transports import UnixTransport

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

## Admin CLI reference

Use only these `pacto-bot-admin` commands. Do not invent commands like `pacto bunker init` or `pacto bot create`; they do not exist yet.

### Bot lifecycle

```bash
# Create a new bot identity + scaffold a Python project in one command
pacto-bot-admin new --scaffold my-bot --backend nsec --relays ws://localhost:7000 --commands hello,help

# Create only the identity config snippet (use this for existing projects)
pacto-bot-admin new my-bot --backend nsec --relays ws://localhost:7000

# Scaffold a handler project for an existing bot identity already in pacto-bot-api.toml
pacto-bot-admin scaffold my-bot --commands hello,help

# Publish a bot profile (kind:0) event
pacto-bot-admin publish-profile my-bot

# Test a NIP-46 bunker connection and pubkey match
pacto-bot-admin test-bunker my-bot
```

**Secret handling:** `new` and `new --scaffold` with `--backend nsec` write an
`nsec` value to `pacto-bot-api.toml`. Do not paste that value or the command
output into chat. For production use `bunker_local` or `bunker_remote`.

### Daemon operations

```bash
# Validate the daemon configuration file
pacto-bot-admin validate-config

# Rotate the HTTP secret token
pacto-bot-admin rotate-http-token --data-dir ~/.local/share/pacto-bot-api

# Show daemon status, connectivity, and registered handlers
pacto-bot-admin status

# Emit structured daemon diagnostics
pacto-bot-admin diagnose
```

### State migration

```bash
# Export bot daemon-local state to JSON
pacto-bot-admin export my-bot > my-bot-state.json

# Import bot daemon-local state from JSON
pacto-bot-admin import my-bot < my-bot-state.json
```

### Run the daemon

```bash
pacto-bot-api --config pacto-bot-api.toml --data-dir ~/.local/share/pacto-bot-api --http-bind 127.0.0.1:8080
```

## Verification checklist

- [ ] Bot imports `from pacto_bot_sdk import Bot`.
- [ ] `Bot(bot_id=...)` is constructed with explicit id.
- [ ] At least one `@bot.command` or `@bot.default` is registered.
- [ ] Handler returns a dict with `event_id` and `action`, or `None`.
- [ ] Example has a manifest if it should be validated by the contract harness.
- [ ] Contract harness passes: `pytest examples/test_examples_contract.py -v`.
- [ ] No invented admin CLI commands appear in docs or docstrings.
- [ ] No real `nsec`, bunker `uri`, or daemon secret tokens are pasted into chat or committed.

## Anti-patterns

- Do not use the hand-written seed `examples/pacto_sdk.py` for new bots; use the generated SDK.
- Do not commit real `nsec` values or secret tokens.
- Do not block the dispatch loop with long synchronous work; use `asyncio.create_task` for deferred actions.

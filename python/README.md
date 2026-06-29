# pacto-bot-api Python SDK

Generated, typed Python SDK for the [`pacto-bot-api`](https://github.com/covenant-gov/pacto-bot-api) daemon.

This package is produced from `schemas/jsonrpc.json` by `cargo xtask codegen`. Bot authors can write handlers against a stable, schema-synced API, and the generated docstrings and examples let an LLM write a complete bot without reading daemon source.

## Installation

```bash
pip install -e python/
```

## Quickstart

A complete bot in ~30 lines:

```python
#!/usr/bin/env python3
from pacto_bot_api import Bot

bot = Bot(bot_id="greeting-bot")


@bot.command("/hello")
async def hello(event, bot):
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": "Hello there! Welcome to Pacto.",
    }


@bot.default
async def unknown(event, bot):
    return {"event_id": event.event_id, "action": "ignore"}


if __name__ == "__main__":
    bot.run()
```

Save it as `my_bot.py` and run:

```bash
python my_bot.py --socket /tmp/pacto.sock
```

## Running the daemon

The Python SDK connects to a running `pacto-bot-api` daemon. For local development:

### Unix socket + nsec (fastest for development)

```bash
# Terminal 1: start the daemon with a raw nsec
export PACTO_SECRET_TOKEN=nsec1yourdevkeyhere
pacto-bot-api --socket /tmp/pacto.sock

# Terminal 2: run your bot
python my_bot.py --socket /tmp/pacto.sock
```

### HTTP + nsec

```bash
# Terminal 1: start the daemon
export PACTO_SECRET_TOKEN=nsec1yourdevkeyhere
pacto-bot-api --http-bind 127.0.0.1:8080

# Terminal 2: run your bot
python my_bot.py --transport http --http-bind 127.0.0.1:8080
```

### Bunker mode (recommended for production)

```bash
# Initialize the bunker directory
pacto bunker init --data-dir ~/.local/share/pacto

# Create a bot identity
pacto bot create --bot-id greeting-bot --data-dir ~/.local/share/pacto

# Start the daemon
pacto-bot-api --data-dir ~/.local/share/pacto --http-bind 127.0.0.1:8080

# Run the bot with the secret stored by the admin CLI
export PACTO_SECRET_TOKEN=$(cat ~/.local/share/pacto/bots/greeting-bot/secret_token)
python my_bot.py --transport http --http-bind 127.0.0.1:8080
```

In bunker mode each bot has its own `secret_token` file at `<data-dir>/bots/<bot-id>/secret_token`.

## The canonical bot loop

Every bot follows the same loop:

1. **Register** via `handler.register` with `bot_ids`, `event_types`, and `capabilities`.
2. **Receive** `agent.event` notifications of type `dm_received`.
3. **Dispatch** the command with `@bot.command("/name")` or `@bot.default`.
4. **Return** a `handler.response` dict with `event_id` and `action`.

Valid actions are:

- `ack` — acknowledge the event.
- `reply` — send `content` back to the user.
- `defer` — acknowledge now, handle asynchronously.
- `ignore` — do nothing.

## High-level `Bot` API

```python
from pacto_bot_api import Bot

bot = Bot(
    bot_id="my-bot",
    event_types=["dm_received"],
    capabilities=["ReadMessages", "SendMessages"],
)
```

### Decorators

- `@bot.command("/hello")` — register a handler for `/hello`. The leading `/` is optional.
- `@bot.default` — fallback handler for unrecognized commands.
- `@bot.status` — callback for `agent.status` notifications.

Handlers receive `(event, bot)` and may be sync or async. Return a response dict or `None`.

### Helper methods on `Bot`

- `await bot.send_dm(recipient, content, reply_to=None)` — send a DM as this bot.
- `await bot.set_profile(name=None, about=None, picture=None)` — update the bot profile.
- `bot.client` — access the low-level `PactoClient` for advanced use.

### Transport resolution

Settings resolve in this order: explicit constructor argument → CLI flag → environment variable → default.

| Setting     | CLI flag        | Environment variable   | Default                              |
|-------------|-----------------|------------------------|--------------------------------------|
| Transport   | `--transport`   | `PACTO_TRANSPORT`      | `unix`                               |
| Socket path | `--socket`      | `PACTO_SOCKET`         | `<data-dir>/pacto-bot-api.sock`      |
| Data dir    | `--data-dir`    | `PACTO_DATA_DIR`       | `~/.local/share/pacto-bot-api`       |
| HTTP bind   | `--http-bind`   | `PACTO_HTTP_BIND`      | `127.0.0.1:9800`                     |
| Secret      | `--secret`      | `PACTO_SECRET_TOKEN`   | `<data-dir>/bot_secret_token`        |

## Low-level `PactoClient` API

For advanced bots, use the generated async client directly:

```python
from pacto_bot_api import PactoClient
from pacto_bot_api.transports import UnixTransport

transport = UnixTransport("/tmp/pacto.sock")
client = PactoClient(transport)
await client.connect()

result = await client.handler_register(
    bot_ids=["my-bot"],
    event_types=["dm_received"],
    capabilities=["ReadMessages", "SendMessages"],
)

async for notification in client.notifications():
    if notification.jsonrpc_method == "agent.event":
        await client.handler_response(
            event_id=notification.event_id,
            action="reply",
            content="Hello!",
        )
```

All JSON-RPC methods have typed Pydantic models:

- `handler.register` → `client.handler_register(...)` → `HandlerRegisterResult`
- `handler.unregister` → `client.handler_unregister()` → `HandlerUnregisterResult`
- `agent.send_dm` → `client.agent_send_dm(...)` → `str`
- `agent.set_profile` → `client.agent_set_profile(...)` → `str`
- `agent.error` → `client.agent_error(...)` (fire-and-forget notification)
- `handler.response` → `client.handler_response(...)` (fire-and-forget notification)

Incoming notifications are validated into `AgentEventParams` and `AgentStatusParams`.

## Capabilities

Common capabilities a bot can request:

- `ReadMessages` — receive `dm_received` events.
- `SendMessages` — reply or send DMs via `agent.send_dm`.

Request only the capabilities your bot needs.

## Examples

- [`python/examples/greeting_bot.py`](examples/greeting_bot.py) — `/hello` reply bot.
- [`python/examples/joke_bot.py`](examples/joke_bot.py) — `/joke` with `defer` and proactive `send_dm`.

Run them against a local daemon:

```bash
python python/examples/greeting_bot.py --socket /tmp/pacto.sock
python python/examples/joke_bot.py --socket /tmp/pacto.sock
```

## Regenerating the SDK

After `schemas/jsonrpc.json` changes, regenerate:

```bash
cargo xtask codegen
```

`tests/schema_sync.rs` fails CI if the generated Python files drift from the schema.

## Development

```bash
python -m venv .venv
source .venv/bin/activate
pip install -e python/
pip install -r examples/requirements.txt
pytest python/tests/
pytest examples/test_examples_contract.py
```

## When to use which layer

- Use `Bot` for most bots. It handles transport selection, registration, command parsing, and response framing.
- Use `PactoClient` directly when you need full control over the JSON-RPC lifecycle or want to integrate with a non-standard event loop.
- Use the generated Pydantic models when building custom tooling around the schema.

## Security notes

- Never commit real `nsec` values or secret tokens.
- In production, use bunker mode with per-bot `secret_token` files.
- The SDK never logs secrets; redact them in your own handlers too.

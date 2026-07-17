# pacto-bot-sdk Python SDK

Generated, typed Python SDK for the [`pacto-bot-api`](https://github.com/covenant-gov/pacto-bot-api) daemon.

This package is produced from `schemas/jsonrpc.json` by `cargo xtask codegen`. Bot authors can write handlers against a stable, schema-synced API, and the generated docstrings and examples let an LLM write a complete bot without reading daemon source.

## Installation

```bash
pip install pacto-bot-sdk
```

## Quickstart

A complete bot in ~30 lines:

```python
#!/usr/bin/env python3
from pacto_bot_sdk import Bot

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

### Admin-managed bot identity

For local development with a config file and admin CLI:

```bash
# Create a new bot identity config snippet
pacto-bot-admin new greeting-bot --backend nsec --relays ws://localhost:7000

# Add the generated snippet to pacto-bot-api.toml, then publish the profile
pacto-bot-admin publish-profile greeting-bot

# Rotate the daemon HTTP secret token
pacto-bot-admin rotate-http-token --data-dir ~/.local/share/pacto-bot-api

# Start the daemon
pacto-bot-api --config pacto-bot-api.toml --data-dir ~/.local/share/pacto-bot-api --http-bind 127.0.0.1:8080

# Run the bot with the rotated token
export PACTO_SECRET_TOKEN=$(cat ~/.local/share/pacto-bot-api/bot_secret_token)
python python/examples/greeting_bot.py --transport http --http-bind 127.0.0.1:8080
```

Per-bot `secret_token` files and bunker-specific lifecycle commands (e.g., `pacto bot create`, `pacto bunker init`) are not yet implemented; use the global `bot_secret_token` file for now.

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
from pacto_bot_sdk import Bot, parse_command

bot = Bot(
    bot_id="my-bot",
    event_types=["dm_received"],
    capabilities=["ReadMessages", "SendMessages"],
)
```

By default, unhandled handler exceptions reply with a friendly error message so
users know the bot is alive. Disable this with `reply_on_error=False` or change
the message with `error_message="..."`. The error text never includes raw
exception details.

### Handler response contract

Handlers return a response dict, `None`, or one of the helpers `bot.ignore` and
`bot.reply`. Valid actions in a response dict are `ack`, `reply`, `defer`, and
`ignore`. When `auto_acknowledge=True` (the default), returning `None` is the
same as returning `bot.ignore(event)` — the SDK sends a terminal
`handler_response(action="ignore")` for you.

```python
from pacto_bot_sdk import Bot

bot = Bot(bot_id="example-bot")


@bot.command("/hello")
async def hello(event, bot):
    return bot.reply(event, "Hello!")


@bot.hears("status")
async def status(event, bot):
    return bot.reply(event, "I am running.")


@bot.event("dm_received")
async def on_dm(event, bot):
    if bot.is_degraded:
        return bot.ignore(event)
    return bot.reply(event, "Got it.")


@bot.dm
async def on_dm_shorthand(event, bot):
    # Returning None becomes ignore when auto_acknowledge is True.
    if event.content.startswith("/ignore"):
        return None
    return bot.reply(event, "Replied via @bot.dm")


@bot.default
async def fallback(event, bot):
    return bot.ignore(event)
```

`bot.reply(event, content)` validates that `content` is a `str` with a UTF-8
length of at most 8192 bytes and raises `ValueError` otherwise. It does not
sanitize content — scrub user input before calling it.

### Decorators

- `@bot.command("/hello")` — register a handler for `/hello`. The leading `/` is optional.
- `@bot.hears("token")` — register a handler for messages whose first token matches *token*.
- `@bot.event("type")` — register a handler for `agent.event` notifications of *type*.
- `@bot.dm` — shorthand for `@bot.event("dm_received")`.
- `@bot.default` — fallback handler for unrecognized commands.
- `@bot.status` — callback for `agent.status` notifications.
- `@bot.rate_limited` — callback for `agent.rate_limited` notifications.

Handlers receive `(event, bot)` and may be sync or async. Return a response dict, `None`, or a helper result.

Use `parse_command(event.content)` to split a message into `command`, `args`, and `flags`.

### Helper methods on `Bot`

- `bot.ignore(event)` — return a terminal `handler_response(action="ignore")` dict.
- `bot.reply(event, content)` — return a terminal `handler_response(action="reply")` dict.
- `await bot.send_dm(recipient, content, reply_to=None)` — send a DM as this bot.
- `await bot.set_profile(name=None, about=None, picture=None)` — update the bot profile.
- `bot.client` — access the low-level `PactoClient` for advanced use.
- `bot.is_degraded` — `True` when the circuit breaker is open and the bot is not dispatching.

### Squad (MLS group) helpers

Bots that join Pacto Squads need `mls_db_path` configured in the daemon and the
appropriate MLS capabilities. Register with `SendGroupMessages`,
`ReceiveGroupMessages`, and `ExitMlsGroup` as needed:

```python
bot = Bot(
    bot_id="squad-bot",
    event_types=["mls_group_message_received"],
    capabilities=["SendGroupMessages", "ReceiveGroupMessages", "ExitMlsGroup"],
)
```

Available helpers:

- `await bot.send_group_message(group_id, content)` — send an encrypted message to the Squad.
- `await bot.is_squad_member(group_id, member_pubkey)` — check whether a pubkey is a Squad member.
- `await bot.exit_squad(group_id)` — publish a self-removal MLS proposal to leave the Squad. Returns the hex event id of the published kind:445 evolution event. The actual removal must be committed by a Squad admin.

### Reconnection resilience

`Bot` retries the initial registration and all runtime reconnects with exponential
backoff, jitter, and a circuit breaker. This keeps the bot alive across daemon
restarts and short network blips without relying solely on Docker/systemd restart
policies.

Configure the retry/circuit behavior via constructor kwargs or CLI flags:

| Setting | Constructor kwarg | CLI flag | Default |
|---------|-------------------|----------|---------|
| Initial backoff | `retry_initial_backoff` | `--retry-initial-backoff` | `1.0` |
| Max backoff | `retry_max_backoff` | `--retry-max-backoff` | `30.0` |
| Jitter ratio | `retry_jitter_ratio` | `--retry-jitter-ratio` | `0.2` |
| Failure threshold | `circuit_failure_threshold` | `--circuit-failure-threshold` | `5` |
| Cooling-off period | `circuit_cooling_off_seconds` | `--circuit-cooling-off-seconds` | `60.0` |
| Degraded log interval | `degraded_log_interval` | `--degraded-log-interval` | `60.0` |

When the circuit breaker opens, the bot logs a single degraded message and then
a short status line at most once per `degraded_log_interval`. It resumes dispatch
automatically after the cooling-off period plus a successful probe.

### Optional extras

Install the `http` extra to pull in `httpx` for bots that call external APIs:

```bash
pip install "pacto-bot-sdk[http]"
```

### Transport resolution

Settings resolve in this order: explicit constructor argument → CLI flag → environment variable → default.

| Setting     | CLI flag        | Environment variable   | Default                              |
|-------------|-----------------|------------------------|--------------------------------------|
| Transport   | `--transport`   | `PACTO_TRANSPORT`      | `unix`                               |
| Socket path | `--socket`      | `PACTO_SOCKET`         | `<data-dir>/pacto-bot-api.sock`      |
| Data dir    | `--data-dir`    | `PACTO_DATA_DIR`       | `~/.local/share/pacto-bot-api`       |
| HTTP bind   | `--http-bind`   | `PACTO_HTTP_BIND`      | `127.0.0.1:9800`                     |
| Secret      | `--secret`      | `PACTO_SECRET_TOKEN`   | `<data-dir>/bot_secret_token`        |

## Logging

The SDK uses a small internal level-aware logger that writes to `stderr`.
Messages are emitted with a prefix like `[my-bot] INFO: message` so they are easy
to follow in Docker or systemd logs.

Control the level with one of (highest precedence first):

1. Constructor argument: `Bot(..., log_level="debug")`
2. CLI flag: `python my_bot.py --log-level debug`
3. Environment variable: `PACTO_LOG_LEVEL` (default: `info`)

Valid levels are `debug`, `info`, `warn`, and `error`.

```bash
PACTO_LOG_LEVEL=debug python my_bot.py
```

At `INFO` you will see connection success, registration success, command routing,
handler responses, and connection state changes. At `DEBUG` you will see full
incoming and outgoing JSON-RPC payloads and raw transport frames.

Handlers can use the same logger:

```python
@bot.command("/hello")
async def hello(event, bot):
    bot.log(f"handling /hello from {event.author[:20]}...")
    return {"event_id": event.event_id, "action": "reply", "content": "Hello!"}
```

## Low-level `PactoClient` API

For advanced bots, use the generated async client directly:

```python
from pacto_bot_sdk import PactoClient
from pacto_bot_sdk.transports import UnixTransport

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
pip install -e python/[dev]
pytest python/tests/
```

## When to use which layer

- Use `Bot` for most bots. It handles transport selection, registration, command parsing, and response framing.
- Use `PactoClient` directly when you need full control over the JSON-RPC lifecycle or want to integrate with a non-standard event loop.
- Use the generated Pydantic models when building custom tooling around the schema.

## Security notes

- Never commit real `nsec` values or secret tokens.
- In production, use bunker mode with per-bot `secret_token` files.
- The SDK never logs secrets; redact them in your own handlers too.

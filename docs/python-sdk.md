# Python SDK seed (`examples/pacto_sdk.py`)

> **Status:** manual seed. This is a hand-written helper for example bots, not
> the eventual generated Python client. The generated client will be derived
> from `schemas/jsonrpc.json`; see
> [`docs/plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md`](plans/2026-06-28-001-feat-python-examples-ci-contract-tests-plan.md).

`examples/pacto_sdk.py` is a single-file, standard-library-only SDK for writing
`pacto-bot-api` handlers in Python. It handles JSON-RPC framing, registration,
lifecycle, shutdown signals, command dispatch, and response helpers so bot
authors can focus on behavior.

## Quick start

```python
import argparse
import asyncio

from pacto_sdk import PactoClient, add_sdk_arguments


async def hello(event, client):
    return client.reply(event["event_id"], "Hello there!")


async def main(argv=None):
    parser = argparse.ArgumentParser(description="Greeting bot.")
    add_sdk_arguments(parser)
    args = parser.parse_args(argv)

    client = PactoClient(
        bot_id=args.bot_id,
        socket_path=args.socket,
        data_dir=args.data_dir,
        transport=args.transport,
        secret=args.secret,
        http_bind=args.http_bind,
    )
    client.on("/hello", hello)
    client.on_default(lambda event, client: client.ignore(event["event_id"]))
    await client.run()


if __name__ == "__main__":
    asyncio.run(main())
```

See `examples/greeting_bot.py` for a complete runnable example.

## Transport selection

### Unix socket (default)

The socket path resolves in this order:

1. `--socket` / `$PACTO_SOCKET`
2. `--data-dir` / `$PACTO_DATA_DIR` → `<data_dir>/pacto-bot-api.sock`
3. Default: `~/.local/share/pacto-bot-api/pacto-bot-api.sock`

### HTTP + SSE

Enable with `--transport http` / `$PACTO_TRANSPORT=http`.

Configuration:

- Bind address: `--http-bind` / `$PACTO_HTTP_BIND` (default `127.0.0.1:9800`)
- Secret token: `--secret` / `$PACTO_SECRET_TOKEN` /
  `<data_dir>/bot_secret_token`

The HTTP transport:

- Registers via `POST /` with `X-Pacto-Bot-Secret`
- Consumes notifications via `GET /events?handler_id=<id>` as SSE
- Attaches `X-Pacto-Handler-Id` automatically for mutating methods
  (`agent.send_dm`, `agent.set_profile`, `agent.error`)

## Command syntax

Incoming `agent.event` content is parsed as:

```
/command arg1 arg2 --flag value --bool
```

The leading `/` is stripped before registry lookup, so `client.on('/hello',
handler)` and `client.on('hello', handler)` both match `/hello` and `hello`.

- Tokens starting with `--` are flags.
- If the next token does not start with `--`, it becomes the flag value.
- Otherwise the flag is treated as boolean `True`.
- Positional tokens are collected in `args`.

Defensive limits: 256 tokens, 1024 bytes per token, 50 args/flags.

## `PactoClient` API

### Constructor

```python
PactoClient(
    bot_id="echo-bot",
    event_types=["dm_received"],
    capabilities=["ReadMessages", "SendMessages"],
    socket_path=None,
    data_dir=None,
    transport="unix",      # or "http"
    secret=None,           # HTTP only
    http_bind=None,        # HTTP only
)
```

### Registry

- `client.on(command, handler)` — register a handler for a command (with or
  without leading `/`).
- `client.on_default(handler)` — fallback handler for unrecognized commands.
- `client.on_status(handler)` — callback for `agent.status` notifications.

Handlers receive `(event, client)` and may be sync or async. A handler that
returns a dict sends it as a `handler.response` notification. Returning `None`
sends nothing.

### Lifecycle

- `await client.run()` — connect, register, and run the dispatch loop until
  SIGINT/SIGTERM.
- `await client.send(method, params)` — send any JSON-RPC notification.

### Response helpers

Return these from command handlers:

- `client.ack(event_id)`
- `client.reply(event_id, content)`
- `client.defer(event_id)`
- `client.ignore(event_id)`

### Notification helpers

These build params dicts for mutating daemon calls:

- `client.send_dm(bot_id, recipient, content, reply_to=None)`
- `client.set_profile(bot_id, name=None, about=None, picture=None)`
- `client.error(bot_id, message, code=None, data=None)`

Use them with `await client.send("agent.send_dm", client.send_dm(...))`.

## CLI helper

`add_sdk_arguments(parser)` adds the common flags to an `argparse.ArgumentParser`:

- `--socket`
- `--data-dir`
- `--bot-id`
- `--transport`
- `--http-bind`
- `--secret`

## Testing

The SDK is exercised by the existing example contract harness:

```bash
pytest examples/test_examples_contract.py -v
```

A focused HTTP+SSE test is also available:

```bash
pytest examples/test_greeting_bot_http.py -v
```

## Notes

- `pacto_sdk.py` and all example bots use only the Python standard library.
- `examples/echo_bot.py` remains the stdlib-only reference implementation.
- Do not log or commit real secret tokens.

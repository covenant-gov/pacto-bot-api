# Pacto Reference Handlers and Examples

> **New bots should use the generated Python SDK.** See
> [`python/README.md`](../python/README.md) and
> [`python/examples/`](../python/examples/) for the current recommended starting
> point. The files in this directory are a legacy standard-library seed used to
> bootstrap the example test suite and the CI contract tests.

## Files

| File | Purpose |
|------|---------|
| `echo_bot.py` | Reference handler using only the Python standard library. |
| `pacto_sdk.py` | Single-file SDK seed used by the legacy example tests. |
| `greeting_bot.py` | ~30-line demonstration of `pacto_sdk.py` (legacy seed). |
| `greeting_bot.manifest.json` | Contract manifest for `greeting_bot.py`. |
| `conftest.py` | pytest fixtures: `daemon`, `handler_client`, `mock_relay`. |
| `test_echo_bot.py` | Integration tests for handler registration and echo behavior. |
| `test-config.toml` | Example daemon config with placeholder keys. |

## Rust example tests

The Rust integration suite also contains focused, readable example tests:

| File | Purpose |
|------|---------|
| `tests/example_http_handler.rs` | HTTP handler registration, SSE notifications, and `handler.response`. |
| `tests/example_multi_bot.rs` | One daemon multiplexing multiple bot identities to separate handlers. |

Run them with:

```bash
cargo test --test example_http_handler --test example_multi_bot
```

## Setup

Requires Python 3.10 or newer.

```bash
cd examples
python -m venv .venv
source .venv/bin/activate
pip install pytest websockets
```

Or install the example dependencies from the repository root:

```bash
pip install -r examples/requirements.txt
```

## SDK seed (`pacto_sdk.py`)

`pacto_sdk.py` is a hand-written, standard-library-only helper that abstracts
JSON-RPC framing, registration, lifecycle, command dispatch, and response
helpers. New example bots should import it rather than copying the plumbing
from `echo_bot.py`.

> **Note:** This is a *manual seed*, not the generated Python client. The
> generated SDK is now available in [`python/`](../python/) and is derived from
> `schemas/jsonrpc.json`. New bots should import from `pacto_bot_api` instead.

Example usage:

```python
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
```

Command syntax parsed from `agent.event` content:

```
/command arg1 arg2 --flag value --bool
```

The leading `/` is stripped before registry lookup, so `client.on('/hello',
handler)` matches both `/hello` and `hello`. Tokens starting with `--` are
flags; if the next token does not start with `--` it becomes the flag value,
otherwise the flag is treated as boolean `True`.

Transport selection:

- **Unix socket** (default): path is resolved from `--socket` / `$PACTO_SOCKET`,
  `--data-dir` / `$PACTO_DATA_DIR`, or `~/.local/share/pacto-bot-api/pacto-bot-api.sock`.
- **HTTP+SSE**: enabled with `--transport http` / `$PACTO_TRANSPORT=http`. The
  default bind is `127.0.0.1:9800` (`$PACTO_HTTP_BIND`). The secret is read from
  `--secret`, `$PACTO_SECRET_TOKEN`, or `<data_dir>/bot_secret_token`.

## Running the echo handler

1. Build and start the daemon:

```bash
cargo build --release
cargo run --bin pacto-bot-admin -- new echo-bot --backend nsec
# Paste the printed [[bots]] snippet into a config file with mode 0o600.
cargo run --bin pacto-bot-api -- --config your-config.toml
```

2. In another terminal run the handler:

```bash
python examples/echo_bot.py --socket ~/.local/share/pacto-bot-api/pacto-bot-api.sock
```

Send a DM starting with `/echo ` to the bot and the handler will echo the rest
back as a reply.

## Running the tests

```bash
pytest examples/test_echo_bot.py -v
```

The test suite:

1. Spawns the Rust daemon with a temporary config and freshly generated keys.
2. Connects a test handler and verifies `handler.register`.
3. Tests `echo_bot.py` end-to-end against a mock daemon Unix socket to verify
   it replies to `/echo` events and ignores others.
4. Verifies the daemon accepts `agent.send_dm` requests.
5. Starts a mock WebSocket relay and confirms the daemon subscribes to it.

## Notes

- `echo_bot.py` intentionally uses only the Python standard library.
- Tests are deterministic and do not require Docker.
- The current daemon transport broadcasts `agent.status` / `agent.event`
  notifications to the handler registry; the reference Python handler implements
  the consumer side of this contract.

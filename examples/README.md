# Pacto Python Reference Handler

This directory contains a reference Python handler (`echo_bot.py`) and pytest
fixtures/tests for the `pacto-bot-api` daemon.

## Files

| File | Purpose |
|------|---------|
| `echo_bot.py` | Reference handler using only the Python standard library. |
| `conftest.py` | pytest fixtures: `daemon`, `handler_client`, `mock_relay`. |
| `test_echo_bot.py` | Integration tests for handler registration and echo behavior. |
| `test-config.toml` | Example daemon config with placeholder keys. |

## Setup

Requires Python 3.10 or newer.

```bash
cd examples
python -m venv .venv
source .venv/bin/activate
pip install pytest websockets
```

Or install from the repo root:

```bash
pip install -r requirements.txt
```

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

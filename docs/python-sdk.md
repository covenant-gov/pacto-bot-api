# Python SDK

The generated Python SDK lives in `python/` and is produced from `schemas/jsonrpc.json` via `cargo xtask codegen`.

> **Migration note:** The temporary hand-written seed SDK at `examples/pacto_sdk.py` has been removed. New bots should use the generated SDK (`from pacto_bot_sdk import Bot`) documented in [`python/README.md`](../python/README.md).

## Quick start

```bash
pip install pacto-bot-sdk
python python/examples/greeting_bot.py --socket /tmp/pacto.sock
```

See [`python/README.md`](../python/README.md) for:

- Installation and quickstart
- Daemon setup (Unix socket, HTTP, bunker mode)
- The canonical bot loop
- `Bot` decorator API
- Low-level `PactoClient` API
- Capability requirements
- All examples

## Legacy seed SDK

The temporary hand-written seed SDK at `examples/pacto_sdk.py` has been removed. The generated `pacto_bot_sdk` package in `python/` replaces it.

## Regeneration

```bash
cargo xtask codegen
```

The generated files under `python/src/pacto_bot_sdk/_generated/` are checked into git. CI enforces schema sync via `tests/schema_sync.rs`.

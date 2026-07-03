# Python SDK

The generated Python SDK lives in `python/` and is produced from `schemas/jsonrpc.json` via `cargo xtask codegen`.

> **Migration note:** `examples/pacto_sdk.py` was a temporary hand-written seed. New bots should use the generated SDK (`from pacto_bot_sdk import Bot`) documented in [`python/README.md`](../python/README.md).

## Quick start

```bash
pip install -e python/
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

`examples/pacto_sdk.py` remains in the repo until migration is complete. It is a single-file, standard-library-only helper used by the original example bots. Do not use it for new bots.

## Regeneration

```bash
cargo xtask codegen
```

The generated files under `python/src/pacto_bot_sdk/_generated/` are checked into git. CI enforces schema sync via `tests/schema_sync.rs`.

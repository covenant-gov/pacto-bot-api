# `pacto-dev-env` Integration Testing

This document describes how to run the Docker-backed integration tests for
`pacto-bot-api` against the shared `pacto-dev-env` services.

## What the tests cover

The dev-env tests live in `tests/dev_env.rs`. They exercise real external
services instead of the in-process mocks used by the default `make test` suite:

- **Nostr relay** at `ws://localhost:7000`
- **EVM node** (Anvil) at `http://localhost:8545`
- **`pacto-bot-admin diagnose`** against a config that points at the relay
- **Full DM round trip** — daemon → relay → handler → daemon → relay

An optional **NIP-46 bunker** is available at `http://127.0.0.1:3001` when the
`bunker` Docker Compose profile is enabled.

## Start the services

The services are defined in the `pacto-dev-env` repository (or the
`dev-setup/docker-compose.yml` in the broader Pacto workspace). From that
directory:

```bash
# Default stack: relay + EVM node
docker compose up -d --build

# Include the optional bunker for NIP-46 signing tests
docker compose --profile bunker up -d --build
```

Verify the stack is up:

```bash
curl -s http://localhost:7000 | head -5
cast block-number --rpc-url http://localhost:8545
```

## Run the gated tests

The dev-env tests are marked `#[ignore]` and also perform a runtime check for
`PACTO_DEV_ENV=1`. They are **never** executed by default `make test`.

```bash
# Default test suite (in-process mocks only, no Docker)
make test

# Dev-env integration tests against the running Docker services
PACTO_DEV_ENV=1 cargo test -- --ignored

# Run only the dev-env file
PACTO_DEV_ENV=1 cargo test --test dev_env -- --ignored
```

If the Docker services are not running, the dev-env tests will fail with a
clear connection-refused message.

## Adding a new dev-env test

1. Place the test in `tests/dev_env.rs`.
2. Add `#[ignore = "requires pacto-dev-env Docker services (set PACTO_DEV_ENV=1)"]`.
3. Begin the test body with:

   ```rust
   if !common::skip_unless_dev_env() {
       return Ok(());
   }
   ```

4. Use `common::dev_relay_url()` and `common::dev_evm_url()` for service
   endpoints so the values stay consistent.
5. Keep tests deterministic and clean up any daemon children or relay state.

## Notes

- Do not run these tests against a production relay or EVM node. They generate
  fresh bot identities and publish real Nostr events.
- The full DM round-trip test creates a temporary data directory and Unix
  socket, starts the daemon, and shuts it down with SIGINT when finished.

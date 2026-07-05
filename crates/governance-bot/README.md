# Governance Snapshot Bot

An example handler for `pacto-bot-api` that reads on-chain governance state
from a Pacto squad and posts encrypted MLS group-message snapshots to a Squad
channel on a configurable cadence.

## What it does

1. Connects to the `pacto-bot-api` daemon over Unix socket or HTTP.
2. Registers with the `SendGroupMessages` capability and publishes the bot's
   MLS KeyPackage so it can be invited to a Squad.
3. On a configurable timer (default daily), reads public governance and
   treasury state from the `pacto-gov` contracts.
4. Formats the data as Markdown.
5. Sends the snapshot into the Squad channel via `agent.send_group_message`.

Failures are retried with exponential backoff; there is no human-paste fallback.

## Configuration

Configuration is loaded from environment variables:

| Variable | Required | Default | Description |
|---|---|---|---|
| `PACTO_GOVERNANCE_RPC_URL` | yes | — | JSON-RPC endpoint (Sepolia or anvil) |
| `PACTO_GOVERNANCE_BOT_ID` | yes | — | Bot identity from `pacto-bot-api.toml` |
| `PACTO_GOVERNANCE_GROUP_ID` | yes | — | Hex-encoded MLS group ID |
| `PACTO_GOVERNANCE_DAEMON_SOCKET` | * | — | Path to `pacto-bot-api.sock` |
| `PACTO_GOVERNANCE_DAEMON_HTTP` | * | — | Daemon HTTP endpoint, e.g. `http://127.0.0.1:9800` |
| `PACTO_GOVERNANCE_HTTP_SECRET` | † | — | HTTP secret token |
| `PACTO_GOVERNANCE_SQUAD_INDEX` | no | `0` | Registry index of the squad |
| `PACTO_GOVERNANCE_CADENCE_SECONDS` | no | `86400` | Seconds between posts |
| `PACTO_GOVERNANCE_CAPTAIN` | no | `0x0` | Captain address for Hats checks |
| `PACTO_GOVERNANCE_CREW_CANDIDATES` | no | — | Comma-separated addresses |
| `PACTO_GOVERNANCE_PROPOSER_CANDIDATES` | no | — | Comma-separated addresses |

`*` One of `PACTO_GOVERNANCE_DAEMON_SOCKET` or `PACTO_GOVERNANCE_DAEMON_HTTP` is required.
`†` Required when using HTTP.

## Local anvil demo

```bash
# 1. Start pacto-bot-api daemon with an MLS-enabled bot identity.
cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml --data-dir ./data

# 2. Deploy a NavePirata squad via DeployNavePirata.s.sol (in pacto-gov) and
#    invite the bot by publishing its KeyPackage and sending the Welcome.

# 3. Configure and run the handler.
export PACTO_GOVERNANCE_RPC_URL=http://localhost:8545
export PACTO_GOVERNANCE_BOT_ID=gov-bot
export PACTO_GOVERNANCE_GROUP_ID=<hex-group-id>
export PACTO_GOVERNANCE_DAEMON_SOCKET=$PWD/data/pacto-bot-api.sock
export PACTO_GOVERNANCE_CAPTAIN=0x...
export PACTO_GOVERNANCE_CREW_CANDIDATES=0x...,0x...
export PACTO_GOVERNANCE_PROPOSER_CANDIDATES=0x...,0x...
cargo run --bin governance-bot
```

## Testing

```bash
cargo test -p governance-bot
```

The end-to-end demo requires a running daemon and a deployed squad; it is gated
behind `#[ignore]` and runs with `PACTO_DEV_ENV=1`.

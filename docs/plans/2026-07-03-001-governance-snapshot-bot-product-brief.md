# Product Brief: Governance Snapshot Bot + Private Agent Path

## What we're building

A bot that reads public governance and treasury state from the Pacto network and posts a clear, formatted summary into a Squad channel on a regular schedule. The bot joins the channel on its own, reads the data on its own, and posts the summary on its own — no human pasting, no manual steps after setup.

Alongside the bot, we're producing a written architecture plan for a future where the same bot (and richer ones like it) runs inside a Trusted Execution Environment (TEE) — a hardened, verifiable compute enclave that protects the bot's keys and private message access from the host operator, cloud provider, and anyone else who shouldn't see them.

## Why this matters

### The problem today

Running a useful Pacto bot requires a lot of heavy infrastructure: Nostr relay connections, encrypted messaging, key custody, persistence. The `pacto-bot-api` daemon already handles that backend for DM-based bots, but two things are still missing:

1. **Squad channel participation.** Bots can't join encrypted group channels (MLS group messaging). They can only do direct messages today.
2. **On-chain governance visibility.** Squad members have to manually check proposal status, treasury balances, and crew changes by reading the contracts themselves. There's no "what's happening in our squad" digest.

The Chones hackathon is a multi-day window to prove both of these work end-to-end in a low-stakes setting, while framing the longer-term destination: a TEE-hosted private agent that can safely read and act on encrypted Squad messages without re-architecting the bot.

### What the bot delivers to Squad members

A Squad member opens their channel and sees a formatted governance snapshot — active proposals with deadlines and vote counts, treasury balances, upcoming crew changes, any active mutinies, and discussion prompts. They can discuss it in-channel immediately instead of each member separately checking the contracts. The snapshot is posted by the bot's own Nostr identity, encrypted as a real MLS group message, and arrives autonomously on a configurable schedule (default: once per day).

### What the TEE brief delivers

A written plan for how the entire bot stack (daemon, signing service, handler) moves into a confidential compute environment where:
- The bot's encryption keys and group state are sealed at rest — the cloud operator can't read them
- Users can verify the bot is running the expected code via attestation (a "prove you're you" challenge-response)
- The bot can safely handle private messages in the future without re-architecting anything built during the hackathon

## Who's involved

| Role | What they do |
|------|-------------|
| **Hackathon team (Ryan et al.)** | Drives the on-chain reader, snapshot formatter, and TEE architecture; owns the hackathon scope |
| **MLS developer** | Owns the daemon-side MLS group messaging integration — the bot's ability to join a Squad channel and post encrypted messages |
| **Bot operator** | Configures the bot identity, manages keys, runs the daemon |
| **Squad members** | Receive the governance snapshot in the channel and discuss it |

## How the bot works (plain English)

```
1. Bot is created and configured (one-time setup)
   ├── Operator creates a bot identity via the admin CLI
   ├── Bot publishes a "key package" to relays (its MLS handshake card)
   └── An existing Squad member invites the bot to the channel

2. Bot joins the channel (automatic)
   ├── Bot receives an encrypted "Welcome" message via Nostr
   ├── Bot processes the Welcome and initializes its group encryption keys
   └── Bot can now encrypt and post messages to the channel

3. Bot posts governance snapshots (recurring)
   ├── Timer fires (default: once per day)
   ├── Bot reads public on-chain governance state from the RPC endpoint
   ├── Bot formats the state as a Markdown summary
   └── Bot posts the summary as an encrypted MLS group message to the channel
```

The bot never reads private Squad messages in Phase 1. It only reads public on-chain state and posts to the channel. This keeps the privacy story honest — the TEE architecture is for the future where the bot does read private messages, and that future is real only if the Phase 1 bot genuinely doesn't.

## What the snapshot contains

The governance snapshot covers everything a Squad member needs to stay informed without checking contracts themselves:

- **Active proposals** — who proposed, what it does, the deadline, current vote counts (yeas/nays), and whether the captain has approved or vetoed
- **Upcoming deadlines** — crew add/remove timelocks that are about to become executable
- **Treasury balance** — ETH and token balances in the squad's Safe
- **Active mutinies** — if a crew mutiny is in progress: who's proposed as the new captain, vote count, and whether it's executed
- **Captain and crew state** — who's active, who's inactive (via Hats Protocol)
- **Discussion prompts** — suggested topics derived from the snapshot data (e.g., "Proposal #3 deadline is in 2 days — discuss")

## Target chain and contracts

The bot reads from **Sepolia** (Ethereum testnet, chain ID 11155111) for the live demo. Local development uses **anvil** (a local testnet) with the same governance contracts deployed. The bot discovers each squad's contract addresses dynamically via the Nave Pirata registry rather than hardcoding them — so it works on any chain where the contracts are deployed.

**Current status:** The infrastructure contracts are deployed and verified on Sepolia (13 addresses). However, zero per-squad governance clones have been bootstrapped yet. A squad must be deployed before the bot has real governance state to snapshot. For the hackathon demo, anvil with deployed contracts is the accepted fallback.

## Phase 1 vs Phase 2

| | Phase 1 (hackathon) | Phase 2 (stretch) |
|---|---|---|
| **What the bot does** | Reads public on-chain state, posts snapshots on a schedule | Also receives `!snapshot` commands from the channel and posts on demand |
| **Message direction** | Send-only (bot → channel) | Bidirectional (bot ↔ channel) |
| **Encryption** | Bot encrypts outgoing messages; doesn't decrypt incoming | Bot decrypts incoming group messages |
| **New capability** | `SendGroupMessages` | Adds `ReceiveGroupMessages` |
| **Risk** | Forward-secrecy gap: no re-keying, so a key compromise exposes the current epoch's messages | Broader: processing other members' MLS commits can advance the group key schedule |
| **Gating** | Must land first | Only if Phase 1 succeeds — if Phase 1 slips, Phase 2 is dropped |

## Security posture

### Key custody

The bot's Nostr signing key follows the daemon's existing model: NIP-46 bunker preferred (keys held remotely in a signing service), `nsec` dev-only (keys held locally, cleared from memory on exit).

MLS encryption keys are a **new key class** — they're Ed25519/X25519 keys used for group encryption, not Nostr event signing. These are **local-only**: a remote NIP-46 bunker cannot service them (different cryptographic curve). For the hackathon, MLS keys are held locally under the dev-only regime. For production, they must be sealed inside the TEE.

### Threat model

The hackathon MLS send-only path holds group encryption keys with no re-keying. If someone steals the bot's `vector-mls.db` file, they can decrypt both already-posted and future channel messages until the group is externally re-keyed. The TEE architecture is the production mitigation — sealed storage protects the key file at rest.

### What we don't commit to for the hackathon

- No MLS re-keying or group management beyond sending the snapshot
- No reading or summarizing private Squad messages (Phase 2 `!snapshot` only)
- No TEE deployment as running code (written architecture brief only)
- No cross-chain reads beyond Sepolia / anvil
- No daemon-side scheduler (the bot handler owns the cadence timer)

## TEE architecture brief

The brief is a standalone document, not code. It covers:

1. **Platform comparison** — AWS Nitro Enclaves, Azure Confidential VMs, Intel SGX: isolation model, sealed storage, attestation flow, packaging complexity, and a recommended primary platform with rationale.

2. **Packaging** — how the daemon, signing service, and bot handler are packaged for each platform. All three run unchanged inside the TEE — the TEE is a deployment target, not a code redesign.

3. **Sealed storage** — how `vector-mls.db` and MLS key material are protected at rest. The `0o600` file permission is preserved; the TEE adds a transparent encryption layer (e.g., LUKS/dm-crypt under the mount point, or platform-native sealing).

4. **Attestation (`!verify` concept)** — a user's client sends a challenge nonce; the TEE produces an attestation report containing the nonce + a measurement hash; the client verifies the report against the platform's root of trust and compares the measurement hash against a published expected value to confirm the expected code is running.

5. **Deployment** — step-by-step deployment for the recommended platform. The follow-up sprint implements this without re-architecting the governance bot.

The key claim: **the TEE inherits the entire bot stack unchanged** — the daemon, the MLS integration, the bot identity, the handler contract, and the signing model. The follow-up work is packaging, deployment tooling, and attestation integration, not code changes.

## Demo success criteria

The hackathon delivers a working demo where:
- The bot joins a real or local anvil Squad channel via MLS Welcome (encrypted group join)
- The bot posts a governance snapshot as an encrypted MLS group message
- The entire flow is autonomous — no human pasting, no manual intervention
- A Sepolia demo is the preferred target; anvil with deployed contracts is the accepted fallback

## Dependencies and prerequisites

| Dependency | Status | Notes |
|---|---|---|
| mdk-core 0.5.2 (MLS library) | Available | Pinned to the same version the Pacto client uses — wire-format interop requires exact match |
| pacto-gov contracts on Sepolia | Deployed (infrastructure) | 13 verified addresses; per-squad clones must be bootstrapped via `DeployNavePirata.s.sol` |
| anvil + deployed contracts | Available | Local dev path; same contracts as Sepolia |
| MLS developer | Confirmed | Owns the daemon-side MLS integration |
| Pacto client (for inviting the bot) | Required | An existing Squad member must invite the bot's npub via the Pacto client to trigger the MLS Welcome |

## Risks

- **Wire-format interop:** The MLS library (mdk-core) must be pinned to the exact version the Pacto client uses. A version mismatch means the bot can't be invited to a Squad. Mitigation: pin the git rev with a comment explaining the constraint.

- **No squads on Sepolia:** Zero per-squad governance clones exist on Sepolia today. A squad must be bootstrapped before the demo has real governance state. Mitigation: anvil with deployed contracts is the local demo fallback.

- **MLS key custody:** MLS keys are local-only and can't be serviced by a remote signing service. If the key file is compromised, the current epoch's messages are exposed. Mitigation: TEE sealed storage is the production path; the hackathon uses the dev-only local regime.

- **Supply chain:** The MLS library is pinned to a git commit with no crates.io fallback. If the repository is deleted or renamed, builds fail. Mitigation: the commit hash is immutable; for production, the crate should be vendored or mirrored.

- **Engine failures:** If the MLS engine crashes mid-operation, the group state may be left corrupt. The bot would need to be re-invited to recover. Mitigation: the daemon detects the corruption, marks the group as poisoned, and returns a clear error rather than retrying indefinitely.

## What we're not building

- A general-purpose AI assistant for Pacto users
- Non-Pacto chat integrations (Discord, Telegram, etc.)
- A no-code bot builder UI
- A daemon-side scheduler (the handler owns the cadence timer)
- Full MLS group management, re-keying, or admin operations
- TEE deployment as running code during the hackathon (architecture brief only)
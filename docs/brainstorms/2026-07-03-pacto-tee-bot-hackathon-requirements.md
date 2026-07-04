---
date: 2026-07-03
topic: pacto-tee-bot-hackathon
---

# Requirements: Pacto Governance Snapshot Bot + TEE-Hosted Private Agent Path

## Summary

Extend `pacto-bot-api` with minimal MLS group participation so a dedicated bot identity can join a Pacto Squad channel and post periodic governance snapshots. The bot reads public on-chain governance and treasury state, formats a Markdown summary, and publishes it into the channel. The hackathon also produces a written architecture for TEE-hosted private agents that can later handle richer private workflows without re-architecting the governance bot.

## Problem Frame

Running useful Pacto bots today requires a lot of heavy infrastructure: Nostr relay pools, encrypted messaging, key custody, and persistence. The `pacto-bot-api` daemon already multiplexes that backend for DM-based bots, but Squad channels and on-chain governance reads are still unbuilt. The Chones hackathon is a low-stakes, multi-day window to prove a concrete bot pattern while framing the longer-term destination: a TEE-hosted private agent that can safely decrypt and act on encrypted Squad messages. The goal is a demonstrable artifact now, not a full platform build, with a clear architecture path for the privacy-sensitive work later.

## Key Decisions

- **Extend the daemon, not bolt on a sidecar.** The cleanest path is to add MLS group participation to `pacto-bot-api` by depending on `mdk-core` (the same library `pacto-app` uses) and reimplementing the ~300-line orchestration layer (keypackage publish → welcome accept → create_message → send_event_to), swapping Tauri coupling for daemon-native storage. This matches the planned Phase 2+ track in `STRATEGY.md` and reuses the same crypto engine as the existing Pacto client.
- **Governance bot reads public state only.** The first bot never decrypts or reads private group messages. It posts snapshots; it does not listen to encrypted chat. This keeps the TEE privacy justification honest. The send-only MLS path still requires Welcome processing and group key-schedule derivation — that is real MLS crypto, not a thin wrapper — but it removes the inbound message ratchet, not the outbound encryption path.
- **Design for TEE, but defer deployment.** The TEE is a deployment target, not a runtime redesign. The daemon, NIP-46 bunker, and bot handler all run inside the TEE as-is — the same code, the same multi-bot multiplexing, the same handler contract. The TEE provides sealed storage for MLS group state and key material, attestation so users can verify the bot's execution environment, and isolation from the host OS / cloud operator. The hackathon produces a written brief covering: which TEE platforms are appropriate, how to package the stack for each, how sealed storage protects MLS state, how attestation works (`!verify` concept), and deployment instructions. No code re-architecture is required — R15's "no re-architecture" claim is correct because the TEE inherits the entire daemon + handler stack unchanged.
- **Bot identity is messaging-only.** The bot does not hold governance signing authority or treasury control. Key custody follows the daemon's existing security model (NIP-46 bunker preferred, `nsec` dev-only).
- **MLS extension makes good on an existing product claim.** The admin CLI's `ReadMessages` capability already references group messages, but no MLS implementation backs it. R8 brings the implementation in line with that existing claim rather than introducing a purely greenfield feature.

## Actors

- **A1. Ryan / Chones hackathon team.** Drives the on-chain reader, snapshot formatter, and TEE architecture; owns the hackathon scope.
- **A2. Pacto client / MLS developer.** Owns the daemon-side MLS integration needed for the bot to join a Squad channel and post messages.
- **A3. Bot operator.** Configures the bot identity in `pacto-bot-api.toml`, manages keys/bunkers, and runs the daemon.
- **A4. Squad members.** Receive the governance snapshot in the Pacto Squad channel and discuss it.

## Requirements

### Governance snapshot bot

- R1. The bot has a dedicated Nostr identity created via `pacto-bot-admin` and configured in `pacto-bot-api.toml`.
- R2. The bot reads public on-chain governance and treasury state from a configurable RPC endpoint. The target chain is **Sepolia** (Ethereum testnet, chain ID 11155111); local development uses anvil (chain ID 31337) with the same `pacto-gov` contracts. The bot discovers per-squad clone addresses via `NavePirataRegistry.deploymentCount()` / `deploymentAt(i)` / `deployment(topHatId)` and reads state from `TreasuryAuthority`, `MutinyModule`, `Quartermaster`, and the squad Safe. ABIs are vendored from `pacto-gov` foundry `out/` artifacts.
- R3. The bot formats a Markdown snapshot covering: active proposals (`TreasuryAuthority.proposal(id)` + `openProposalOf`), upcoming deadlines (`proposal.deadline`, `Quartermaster.pendingCrewAddAt/RemoveAt`), treasury/Safe balance summary (`eth_getBalance` + ERC-20 `balanceOf`), active mutinies (`MutinyModule.activeMutinyId` + `mutiny(id)`), captain/crew state (Hats `wearerStatus`), and suggested discussion prompts derived from the snapshot data.
- R4. The bot posts the formatted snapshot into a Pacto Squad channel as an encrypted MLS group message.
- R5. Snapshot cadence is configurable, defaulting to once per day. The handler owns the cadence timer (e.g., `tokio::time::interval` or external cron) and the RPC endpoint configuration. Cadence and RPC config are not part of the daemon's config schema.
- R6. Phase 1 does not read or decrypt private Squad messages; it only reads public on-chain state and posts to the channel. Phase 2 (stretch) adds inbound MLS group message decryption for the `!snapshot` command — see Phase 2 stretch goal below.
- R7. The bot must post the snapshot autonomously into the Squad channel via the daemon's MLS group send capability. Human-paste fallback is not an acceptable demo outcome.

### Daemon MLS extension

- R8. The daemon gains MLS group participation by depending on `mdk-core` 0.5.2 at git rev `f46875ec` (the exact version `pacto-app` uses, pinned for wire-format interop). R8 is scoped to three orchestration steps: (a) publish a KeyPackage (kind:443, hex-encoded) and receive + process a Welcome for one group, (b) persist MLS group state + key material per bot identity, (c) encrypt + publish group messages via `engine.create_message()` + `client.send_event_to()`. This is a new cryptographic subsystem unrelated to the existing NIP-44/59 gift-wrap DM path, not a reuse of existing signing. The `mdk` engine is `!Send`; all engine calls must run on `spawn_blocking` with scoped Arcs, following the pattern demonstrated in `pacto-app/src-tauri/src/mls.rs`.
- R9. MLS group state and key material are scoped to the bot identity and never logged or returned in JSON-RPC errors. The daemon owns all MLS keys and the `vector-mls.db` SQLite file; the handler never touches MLS private material. The `vector-mls.db` file is created with `0o600` permissions, consistent with the daemon's existing config-file enforcement. MLS keys are a new key class: Ed25519 credential/signature keypair + X25519 leaf encryption keypair + per-epoch derived secrets, all generated by OpenMLS inside mdk. These keys are local-only — a remote NIP-46 bunker cannot service them (curve mismatch: MLS uses Ed25519/X25519, NIP-46 uses secp256k1 Schnorr; OpenMLS needs raw private keys in-process for HPKE and key-schedule derivation). For the hackathon, MLS keys are held locally in the daemon under the dev-only regime. For the TEE brief (R14), the `vector-mls.db` file and in-memory MLS keys are protected by the TEE's sealed storage and isolation guarantees.
- R9a. Threat model: the hackathon MLS send-only path holds group encryption keys with no re-keying (deferred per Scope Boundaries). Compromise of the bot's `vector-mls.db` exposes future channel message encryption keys until an external re-key outside the bot's control. The TEE architecture (R14) is the production mitigation path. Running the handler in a WASM container to further protect the key file is a potential future hardening, but WASM is out of scope and deferred.
- R10. The MLS extension is exposed to handlers through a new capability (e.g. `SendGroupMessages`) distinct from `SendMessages` and a new JSON-RPC method added to `schemas/jsonrpc.json`. The daemon enforces this capability via `is_authorized(handler_id, bot_id, "SendGroupMessages")` before any group send.
- R11. Generated Rust types (`src/*_generated.rs`) and the Python SDK (`python/`) are regenerated from the updated schema, not hand-edited.
- R12. The MLS extension follows existing transport patterns: it works for handlers connecting over both Unix socket and HTTP transports.

### TEE architecture

- R13. The TEE-hosted private agent architecture is documented in a standalone brief covering: which TEE platforms are appropriate (AWS Nitro Enclaves, Azure Confidential VMs, SGX, etc.), how to package the daemon + bunker + handler stack for each platform, how sealed storage protects MLS group state and key material at rest, how attestation works (`!verify` concept with challenge-response, root of trust, and measurement hash), and deployment instructions / reference architecture.
- R14. The TEE design explains how the bot's Nostr/MLS keys and group state are protected by the TEE's sealed storage and isolation guarantees. The daemon, bunker, and handler run unchanged inside the TEE — the TEE is a deployment target, not a code redesign.
- R15. The TEE design is written so a follow-up sprint can implement it without re-architecting the governance snapshot bot. This claim is correct: the TEE inherits the entire daemon + handler stack unchanged, including the mdk MLS integration, the bot identity, the handler contract, and the signing model. The follow-up work is packaging, deployment tooling, and attestation integration — not code changes to the daemon or handler.
- R16. The TEE brief includes a `!verify`-style attestation concept for users to confirm the bot's execution environment. The concept covers: attestation freshness via challenge-response (nonce tied to the request), the root of trust / attestation verification service the user's client checks against, and the measurement/hash the user compares against to confirm the expected code is running.

### Hackathon artifact and process

- R17. The hackathon delivers a working demo of the governance snapshot bot posting autonomously into a real Pacto Squad channel via the daemon's MLS group send capability. The bot must join the channel via MLS Welcome and post an encrypted group message with no human intervention.
- R19. Code changes follow repo conventions: `cargo xtask codegen` regenerates types, `cargo test` passes (not just `make validate`, which runs only fmt-check and clippy), and `make validate` passes. Tests cover new JSON-RPC methods, MLS redaction, and the actual MLS encryption path via a mock MLS group peer in `tests/support/mock_mls_peer.rs` — adapted from the pacto-app smoke test (`pacto-app/src-tauri/src/mls.rs:1578–1740`), using real `mdk-core` with ephemeral in-memory SQLite (`MdkSqliteStorage::new(":memory:")`), no Docker, no disk, no network required for the core Welcome-exchange + encrypted-message validation. Real-MLS integration tests against a live relay are gated behind `#[ignore]` + `PACTO_DEV_ENV=1`.
- R20. No production secrets (real `nsec`, bunker URI, HTTP token, or MLS group state/key material) are committed or logged.

### Phase 2 stretch goal: `!snapshot` interactive command

- R21. Phase 2 adds a `!snapshot` command handler: the bot receives inbound MLS group messages, decrypts them via `engine.process_message()`, detects `!snapshot` in the plaintext, and responds by posting the current governance snapshot on demand.
- R22. Phase 2 requires a new `Kind::MlsGroupMessage` (kind:445) subscription, separate from the GiftWrap subscription that carries Welcomes. The daemon runs a live-event dispatch loop with h-tag extraction, membership filtering, skip-own-events guard, and `spawn_blocking` for `engine.process_message()`.
- R23. Phase 2 adds a `ReceiveGroupMessages` capability alongside `SendGroupMessages` (R10), enforced via `is_authorized` before the daemon delivers decrypted group messages to the handler.
- R24. Phase 2 expands the R9a threat model: receiving messages means the bot processes other members' MLS commits, which can advance the group key schedule (inbound ratchet). The daemon holds the engine across an inbound event loop, not just outbound send moments — a broader window of engine ownership and group-state mutation.
- R25. Phase 2 is gated on Phase 1 success: R8's send-only orchestration (KeyPackage publish, Welcome accept, create_message, send_event_to, state persist) must land and be demonstrated first. If R8 slips, Phase 2 is dropped, not layered on top. Estimated additional effort: ~150-250 lines of new orchestration on top of R8's ~300-line send-only layer, reusing the same mdk-core engine and `!Send`/`spawn_blocking` pattern. Reference implementation: `pacto-app/src-tauri/src/lib.rs:1954-2512` (live dispatch handler).

## Key Flows

- F1. Periodic snapshot posting
  - **Trigger:** Configurable timer fires (default daily).
  - **Actors:** A1, A3, A4.
  - **Steps:**
    1. Handler's own timer fires (default daily).
    2. Handler calls an on-chain RPC to read governance and treasury state (current-state read, no diffing).
    3. Handler formats the snapshot as Markdown.
    4. Handler calls the new daemon JSON-RPC method to send the snapshot as an MLS group message to the Squad channel.
  - **Outcome:** Squad members see the snapshot in the channel.

- F2. Bot joins a Squad channel
  - **Trigger:** An existing Squad member adds the bot's npub via the Pacto client, which gift-wraps an MLS Welcome to the bot.
  - **Actors:** A2, A3.
  - **Steps:**
    1. Bot publishes a `Kind::MlsKeyPackage` event to relays via `engine.create_key_package_for_event()`.
    2. An existing member adds the bot to the group; the Pacto client gift-wraps a `Kind::MlsWelcome` and sends it to the bot's npub.
    3. The daemon's live GiftWrap subscription receives and unwraps the Welcome, detects `MlsWelcome`, and calls `engine.process_welcome()` then `engine.accept_welcome()`.
    4. Group state (epoch secrets, group metadata) is persisted per bot identity.
  - **Outcome:** The bot can encrypt and publish group messages to the channel via `engine.create_message()` + `client.send_event_to()`.

- F4. `!snapshot` command handler (Phase 2 stretch)
  - **Trigger:** A Squad member posts `!snapshot` in the channel.
  - **Actors:** A4, A3.
  - **Steps:**
    1. Daemon's `Kind::MlsGroupMessage` (kind:445) subscription receives the encrypted group message.
    2. Daemon extracts the group wire ID from the `h` tag, checks membership, and skips own events.
    3. Daemon calls `engine.process_message()` on `spawn_blocking` to decrypt.
    4. Daemon delivers the plaintext to the handler via a new `agent.event` notification with the `ReceiveGroupMessages` capability.
    5. Handler detects `!snapshot` in the plaintext and triggers the same read + format + send flow as F1.
  - **Outcome:** The bot posts the current governance snapshot on demand, demonstrating the daemon's interactive event-receive dispatch path.
  - **Covered by:** R21, R22, R23, R24, R25


## Scope Boundaries

### Deferred for later
- Bot reading or summarizing private encrypted Squad messages beyond the `!snapshot` command (Phase 2 stretch, R21–R25). Full message summarization and richer private workflows remain deferred.
- TEE deployment as running code during the hackathon (design only).
- Bot holding governance signing authority or treasury execution authority.
- Full MLS group management, re-keying, or decryption support beyond what is needed to send the snapshot.
- Running the handler in a WASM container to further protect the MLS key file (potential future hardening).
- Cross-chain governance reads beyond the first target chain and contract set.

### Outside this product's identity

- A general-purpose AI assistant for Pacto users.
- Non-Pacto chat integrations (Discord, Telegram, etc.).
- A no-code bot builder UI.

## Dependencies / Assumptions

- A Pacto client/MLS developer is available to own the daemon-side MLS integration (confirmed in dialogue).
- Pacto Squad channels use MLS for encrypted group messaging.
- The target chain for the first governance snapshot is **Sepolia** (Ethereum testnet, chain ID 11155111). Public RPC endpoints are available (e.g., `ethereum-sepolia-rpc.publicnode.com`). Local development uses an anvil node at `http://localhost:8545` (chain ID 31337) with the same `pacto-gov` contracts deployed via `Deploy.s.sol`.
- The `pacto-gov` infrastructure contracts (NavePirataFactory, NavePirataRegistry, master copies of MutinyModule/Quartermaster/TreasuryAuthority/SquadAdmin, Hats, Safe bundle) are deployed and verified on Sepolia at the addresses in `pacto-gov/deployments/11155111/full-system.json`. However, **zero per-squad governance clones have been bootstrapped on Sepolia yet** (`NavePirataRegistry.deploymentCount()` returns 0). A squad must be deployed via `DeployNavePirata.s.sol` before the bot has real governance state to snapshot.
- The per-squad clone contracts expose all R3 snapshot fields: `TreasuryAuthority.proposal(id)` for proposals + deadlines, `MutinyModule.activeMutinyId()` + `mutiny(id)` for mutinies, `eth_getBalance(safe)` + ERC-20 `balanceOf(safe)` for treasury, `Quartermaster.pendingCrewAddAt/RemoveAt` for timelock deadlines, Hats Protocol `wearerStatus` for crew/sponsor state. Squad discovery via `NavePirataRegistry.deploymentCount()` → `deploymentAt(i)` → `deployment(topHatId)`.
- Governance ABIs are not shipped in `pacto-app`; the bot must vendor or generate ABIs from `pacto-gov`'s foundry `out/` artifacts. The bot must resolve per-squad clone addresses dynamically via the registry rather than hardcoding, since addresses differ between anvil and Sepolia.
- The MLS implementation is `mdk-core` (Marmot Development Kit, `github.com/marmot-protocol/mdk` — the same repo previously referenced as `github.com/parres-hq/mdk`, which is a GitHub org rename redirect). `pacto-app` currently uses mdk-core 0.5.2 at git rev `f46875ec`. mdk-core 0.8.0 (latest on crates.io) is **wire-format incompatible** with 0.5.2: KeyPackage and Welcome content encoding changed from hex (no encoding tag) to base64 (mandatory encoding tag), and KeyPackage migrated from kind:443 to addressable kind:30443. A 0.8.0 bot cannot be invited to a 0.5.2 Squad and vice versa. The daemon must use mdk-core 0.5.2 at the same git rev as pacto-app to guarantee interop. Upgrading to 0.8.0 requires coordinating a pacto-app release alongside the daemon.
- MLS key custody differs from the Nostr signing key model: MLS credential and init keys are local private keys used for Welcome decryption and group-key derivation, not Nostr event-signing operations. A remote NIP-46 bunker cannot service MLS key operations. For the hackathon, MLS keys are held locally under the dev-only regime; for the TEE brief (R14), production MLS key custody must be sealed inside the TEE.

## Outstanding Questions

### Deferred to planning
- Q1. Which chain and contract addresses are the first snapshot target? → **Sepolia** (Ethereum testnet, chain ID 11155111). Infrastructure contracts are live and verified; per-squad clones must be bootstrapped via `DeployNavePirata.s.sol` before the bot has real state to read. Local development uses anvil (chain ID 31337) with the same contracts. Contract addresses are in `pacto-gov/deployments/11155111/full-system.json` and `pacto-app/src/lib/evm/pacto-protocol-addresses.json`.
- Q2. Which RPC endpoint is used for reading on-chain state? → `http://localhost:8545` for local development; `ethereum-sepolia-rpc.publicnode.com` (or another public Sepolia RPC) for the demo.
- Q3. What is the exact Pacto Squad channel invitation / MLS join flow? → **Reuse the existing mdk flow:** an existing member adds the bot's npub via the Pacto client, which gift-wraps an MLS Welcome (`Kind::MlsWelcome` inside NIP-59 GiftWrap) to the bot. The daemon's MLS extension receives the GiftWrap, detects `MlsWelcome`, calls `engine.process_welcome()` then `engine.accept_welcome()`, and initializes group state. The bot must publish a `Kind::MlsKeyPackage` event before it can be invited. Reference implementation: `pacto-app/src-tauri/src/mls.rs` smoke test at lines 1578–1740.
- Q4. What is the exact Markdown snapshot template? (Shape emerges from the chosen contracts and what the Squad cares about.)
- Q5. How are failed automatic posts handled? (Retry with backoff, log and alert. No human-paste fallback — the bot must post autonomously.)
- Q6. What rate limits and error surfaces should the new JSON-RPC method have?
- Q7. Which TEE platform is the design targeting? (Azure Confidential Computing, AWS Nitro Enclaves, SGX, etc.)

## Sources / Research

- `STRATEGY.md` — lists planned Phase 2+ tracks: MLS group participation, on-chain governance reads/writes, webhook delivery.
- `CONCEPTS.md` — defines bot identity, handler, capability, and transport abstractions the governance bot will extend.
- `schemas/jsonrpc.json` — source of truth for the handler-facing JSON-RPC contract; the MLS post method will be added here.
- `schemas/config.json` — source of truth for daemon configuration; the bot identity will be configured here. Snapshot cadence and RPC endpoint configuration are handler-owned and are not part of this schema.
- `pacto-gov` repository (`~/src/covenant-gov/pacto-gov/`) — Nave Pirata governance contracts. Sepolia deployment artifacts at `deployments/11155111/full-system.json` (13 verified on-chain addresses). Per-squad clones deployed via `script/DeployNavePirata.s.sol`. Read methods: `TreasuryAuthority.proposal(id)`, `MutinyModule.activeMutinyId()` + `mutiny(id)`, `NavePirataRegistry.deploymentCount()` / `deploymentAt(i)` / `deployment(topHatId)`, `Quartermaster.pendingCrewAddAt/RemoveAt`. ABIs in foundry `out/` artifacts.
- `pacto-app` address book (`~/src/covenant-gov/pacto-app/src/lib/evm/pacto-protocol-addresses.json`) — canonical Sepolia contract addresses mirroring `pacto-gov/deployments/11155111/full-system.json`, plus SquadSponsor and Safe bundle addresses. Embedded in Rust via `src-tauri/src/evm/pacto_chain_config.rs`.
- `signal-bot-tee` repository — reference TEE bot pattern for the architecture brief.
- `pacto-app` (`~/src/covenant-gov/pacto-app/`) — reference MLS implementation. `src-tauri/src/mls.rs` contains the orchestration layer (keypackage publish, welcome accept, create_message, send_event_to) and a self-contained smoke test (lines 1578–1740) with no Tauri coupling. Uses `mdk-core` 0.5.2 at git rev `f46875ec` from `github.com/marmot-protocol/mdk` (same repo; `github.com/parres-hq/mdk` is an org-rename redirect).
- `mdk-core` (Marmot Development Kit, `github.com/marmot-protocol/mdk`) — MLS-over-Nostr library wrapping OpenMLS. pacto-app uses 0.5.2 at git rev `f46875ec`; 0.8.0 on crates.io is wire-format incompatible (hex→base64 content encoding, kind:443→kind:30443 key packages). The daemon must pin 0.5.2 for interop. Key API: `create_key_package_for_event`, `process_welcome`, `accept_welcome`, `create_message`, `process_message`.

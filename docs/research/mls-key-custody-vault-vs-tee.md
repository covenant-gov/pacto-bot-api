# Research: MLS Key Custody — Vault Injection vs TEE Sealed Storage

Research date: 2026-07-03
Context: `pacto-bot-api` MLS group messaging extension (governance snapshot bot hackathon)

## Problem

The `pacto-bot-api` daemon's MLS extension introduces a new key class: MLS encryption keys (Ed25519 credential/signature keypair, X25519 leaf encryption keypair, per-epoch derived secrets). These keys are generated and held in-process by OpenMLS inside the `mdk-core` engine. Unlike Nostr signing keys, which can be serviced remotely by a NIP-46 bunker, MLS keys cannot be delegated to a remote signing oracle. This creates a key custody gap: where do the keys live, how are they protected at rest, and how are they protected in use?

## Why a NIP-46 bunker cannot service MLS keys

Nostr signing via a NIP-46 bunker works because signing is a single round-trip: the daemon sends a message hash to the bunker, the bunker returns a signature, the private key never leaves the bunker. The bunker is a remote oracle — input in, output out, key stays put.

MLS encryption doesn't work that way. OpenMLS needs raw private key material in-process to perform HPKE (Hybrid Public Key Encryption) operations as part of key schedule derivation — the multi-step cryptographic chain that derives epoch secrets from group state. You can't delegate "derive the next epoch secret" to a remote service because the engine performs compound cryptographic operations internally, using the private keys as part of each step. The keys aren't just signing inputs — they're ingredients in a cooking process that happens inside the engine's kitchen.

Curve mismatch compounds this: MLS uses Ed25519/X25519, NIP-46 uses secp256k1 Schnorr. Even if the protocol allowed remote MLS operations, the bunker's secp256k1 keys are the wrong curve type.

## Vault injection approach

A secrets vault (HashiCorp Vault, AWS Secrets Manager, Azure Key Vault, etc.) can protect MLS keys at rest — store them encrypted in the vault instead of in a plaintext `vector-mls.db` file on disk. At daemon startup, the daemon authenticates to the vault, fetches the keys, and loads them into the OpenMLS engine in memory.

### Architecture

```
Startup:
  1. Daemon authenticates to vault (Vault token, IAM role, K8s service account)
  2. Daemon fetches MLS key material from the vault
  3. Daemon loads keys into OpenMLS engine in memory
  4. Daemon fetches encrypted vector-mls.db (or reconstructs from vault-stored state)
  5. If encrypted, daemon decrypts using a data key from the vault
  6. Engine is ready — keys are in memory, db is decrypted

Runtime:
  - Engine holds keys in memory for the daemon's lifetime
  - All MLS operations use the in-memory keys
  - Vault is not consulted again

Shutdown:
  - Daemon flushes and re-encrypts vector-mls.db
  - Keys are (hopefully) zeroized from memory
  - On next startup, repeat
```

### What it protects

- **Keys at rest:** Protected. Keys are encrypted in the vault, not sitting in a plaintext SQLite file on the daemon's host.
- **Access control:** Vault policies can restrict which identities can fetch the keys, adding an authz layer beyond file permissions.
- **Audit:** Vault logs key access events, providing a trail of who fetched MLS keys and when.

### What it doesn't protect

- **Keys in memory:** Exposed. Once injected, the keys live in the daemon's heap for the daemon's lifetime. A core dump, debugger attach, or `/proc/<pid>/mem` read exposes them. This is the same attack surface as the existing `nsec` dev-only backend.
- **Host operator:** The host operator (who controls the VM or machine the daemon runs on) can dump daemon memory at any time and extract the keys.
- **Cloud provider:** The cloud provider controls the hypervisor and can theoretically read guest VM memory.
- **No verifiable execution:** A vault doesn't let users verify what code is running. The daemon could be modified to exfiltrate keys after fetching them from the vault.

## TEE sealed storage approach

A Trusted Execution Environment (TEE) — AWS Nitro Enclaves, Azure Confidential VMs, Intel SGX — protects keys at rest and in use. The daemon runs inside the TEE; TEE memory is encrypted and isolated from the host OS and hypervisor.

### What it protects

- **Keys at rest:** Protected via sealed storage (platform-native sealing: `sgx_seal_data` for SGX, EBS attach with confidential-disk for Nitro Enclaves, OS-level encryption-at-rest for Azure CVMs).
- **Keys in memory:** Protected. TEE memory is encrypted and inaccessible to the host OS, cloud provider, or hypervisor. The daemon's heap is inside the TEE.
- **Host operator:** Cannot read TEE memory. The host operator provides compute but cannot inspect the enclave's state.
- **Cloud provider:** Cannot read TEE memory. The hypervisor sees encrypted pages.
- **Verifiable execution:** Attestation lets users verify the daemon is running the expected code via a challenge-response protocol with a measurement hash.

### What it doesn't protect

- **Supply chain:** If the TEE's root of trust is compromised (e.g., a compromised attestation service), attestation can be spoofed.
- **Side-channel attacks:** SGX historically has side-channel vulnerabilities (Spectre-class, cache timing). Nitro Enclaves and Azure CVMs have a larger attack surface (full VM) but are isolated at the hypervisor level.
- **Operational complexity:** Packaging, deployment, and attestation integration are non-trivial. The TEE brief (`docs/tee-private-agent-architecture.md`, planned) covers this.

## Comparison

| | Vault | TEE |
|---|---|---|
| Keys at rest | Protected (encrypted in vault) | Protected (sealed storage) |
| Keys in memory | Exposed (daemon's heap) | Protected (TEE memory encrypted + isolated) |
| Host operator can read keys | Yes (can dump daemon memory) | No (TEE memory inaccessible to host OS) |
| Cloud provider can read keys | Yes (controls the VM) | No (TEE memory encrypted from hypervisor) |
| Verifiable execution | No | Yes (attestation with measurement hash) |
| Complexity | Medium | High |
| Operational dependency | Vault availability at startup | TEE-compatible hardware / VM type |
| Key rotation | Vault-native (rotate secret, restart daemon) | Requires re-key within MLS group or re-seal |
| Audit | Vault access logs | TEE attestation logs |

## Vault + TEE combined (production target)

The strongest posture combines both: the vault stores the master key, the TEE authenticates to the vault at startup using attestation-derived credentials, unwraps the MLS state in TEE memory, and runs the engine with memory that the host operator and cloud provider can't read.

```
Startup (vault + TEE):
  1. TEE boots with attestation report (proves it's the expected daemon code)
  2. Vault verifies attestation, releases MLS key material to the TEE
  3. TEE loads keys into OpenMLS engine in TEE-encrypted memory
  4. TEE decrypts sealed vector-mls.db using a data key from the vault
  5. Engine is ready — keys in TEE memory, db decrypted in TEE

Runtime:
  - All MLS operations use keys in TEE-encrypted memory
  - Host operator and cloud provider cannot read keys
  - Attestation lets users verify the environment

Shutdown:
  - TEE re-seals vector-mls.db
  - Keys zeroized from TEE memory
  - On next startup, re-attest to vault
```

## Phased hardening path

| Phase | Key storage | Keys in memory | Complexity | When |
|---|---|---|---|---|
| Hackathon | `vector-mls.db` at `0o600`, local dev-only | Exposed | Low | Now |
| Intermediate | Vault (encrypted at rest, injected at startup) | Exposed | Medium | Post-hackathon hardening |
| Production | TEE sealed storage | Protected (TEE memory) | High | Production deployment |
| Strongest | Vault + TEE (attestation-gated key release) | Protected (TEE memory) | High | Security-critical deployments |

## For the hackathon

Adding vault integration is a real option if better key hygiene is desired without the full TEE packaging effort. The tradeoff is operational complexity — vault availability becomes a daemon startup dependency, and vault auth credentials themselves need protection (chicken-and-egg: what protects the vault token?). For a multi-day hackathon demo running on a laptop with anvil, the dev-only local regime (keys in `vector-mls.db` at `0o600`) is the pragmatic choice. The vault approach is a good intermediate hardening step worth documenting in the TEE architecture brief as the phased path from "hackathon local keys" to "production TEE-sealed keys."

## Open question: zeroization

The daemon's existing `nsec` backend wraps key bytes in `zeroize` so they are cleared from memory on drop. OpenMLS's internal key buffers are not verified to zeroize on drop — the `mdk-core` engine holds key material in its own data structures, and whether those structures implement `Zeroize` is an implementation detail of OpenMLS that has not been audited. If they don't, keys persist in freed heap memory until the allocator reuses the pages. The vault approach doesn't change this (keys are still in the daemon's heap after injection). The TEE approach mitigates it (freed pages in TEE memory are still encrypted). A `Zeroizing` wrapper around the engine's key material would close this gap but requires either OpenMLS cooperation or a custom wrapper that intercepts key material before it enters the engine.

## Sources

- `docs/brainstorms/2026-07-03-pacto-tee-bot-hackathon-requirements.md` — R9, R9a, R14 (MLS key custody, threat model, TEE sealed storage)
- `docs/brainstorms/2026-07-03-pacto-tee-bot-hackathon-review-questions.md` — Q5 resolution (MLS key custody: local-only, NIP-46 bunker cannot service, TEE for production)
- `docs/key-and-secret-security.md` — existing key hygiene policy (`zeroize` for nsec, `0o600` for config files)
- `pacto-app/src-tauri/src/mls.rs` — reference MLS implementation (engine holds `Arc<MDK<MdkSqliteStorage>>`, keys generated by OpenMLS in-process)
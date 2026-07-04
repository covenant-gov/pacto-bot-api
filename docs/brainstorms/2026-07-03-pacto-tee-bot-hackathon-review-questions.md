## Q1. How should we scope R8 given the MLS join flow may not exist yet? — RESOLVED

Requirements doc: `docs/brainstorms/2026-07-03-pacto-tee-bot-hackathon-requirements.md`
Review date: 2026-07-03
Reviewers: coherence, feasibility, product-lens, security-lens, scope-guardian, adversarial

---

## Q1. How should we scope R8 given the MLS join flow may not exist yet?

**Context:** R8 prices the daemon MLS extension as "minimal" on the assumption that the Pacto client already ships an MLS Welcome→group-state flow the daemon just wires to. Q3 then admits the flow may not exist and the Pacto dev will "implement the minimal version needed for the demo." If the flow is absent, R8 balloons from "accept a Welcome" into "design + implement an MLS group join from scratch." Additionally, even send-only MLS requires Welcome processing and group key-schedule derivation — that is real MLS crypto, not a thin wrapper. And no MLS library exists in the daemon today (Cargo.toml has only nostr-sdk/nip44/nip59).

**The core tension:** "Minimal MLS" is honest only if the join flow already exists. If it doesn't, the scope is a new crypto subsystem.

**Option A — Gate R8 on a pre-hackathon feasibility check.** Before committing to automated posting, confirm whether the Pacto client ships an MLS Welcome→group-state flow. If it does, R8 stays "minimal." If it doesn't, re-scope R8 as a separate workstream with its own owner and timebox.
- Pro: Prevents under-allocating and missing the demo window.
- Con: Adds a pre-hackathon dependency check that could delay the start.

**Option B — Accept the larger scope now and name the MLS library.** Add a dependency/assumption entry naming the MLS implementation (e.g. `openmls`) and explicitly scope R8 to: (a) receive + process a Welcome for one group, (b) persist group state + key material per bot identity, (c) encrypt + publish group messages. State that this is a new cryptographic subsystem unrelated to the NIP-44/59 path.
- Pro: Honest about the real scope; implementer plans correctly from day one.
- Con: Raises the perceived hackathon commitment, which may push toward the fallback path.

**Option C — Cut to human-in-the-loop fallback as the primary demo path.** Defer all MLS daemon work to a post-hackathon sprint. The hackathon delivers the on-chain reader + snapshot formatter + TEE architecture brief, with human-paste posting as the demo.
- Pro: Lowest risk; guarantees a deliverable.
- Con: Proves none of the MLS integration value, which is the hackathon's headline artifact.

**How to decide:** The answer depends on whether the Pacto client already has MLS support. If you can check that before the hackathon starts, Option A is the safest. If you cannot check, Option B is the most honest default. Option C is the fallback if MLS integration proves too large for the window.

**Resolution:** Investigated `pacto-app` MLS implementation. The join flow exists — pacto-app ships a complete MLS group-messaging flow in Rust built on `mdk-core` (Marmot Development Kit, wrapping OpenMLS). The flow is tightly coupled to Tauri's AppHandle, but the crypto engine (`mdk-core`) is library-shaped and directly reusable. The daemon depends on `mdk-core` 0.5.2 (pinned for wire-format interop) and reimplements the ~300-line orchestration layer. Option B chosen. Requirements doc updated: R8 names the library, scopes the three steps, and references the pacto-app smoke test as implementation reference.

---

## Q2. Is Sophia a real, deployed chain with governance contracts, or is it aspirational? — RESOLVED

**Context:** The requirements doc asserts Sophia as the target chain, but the feasibility and product reviewers found zero references to "Sophia" anywhere in the codebase, `docs/GETTING_STARTED.md`, dev-env configs, or repo docs. The only verified chain is anvil at `http://localhost:8545` (chain 31337) with `pacto-gov` "Nave Pirata" contracts. The doc also sends conflicting signals: the Assumptions section says "a public RPC endpoint is available," while Q2 says the production Sophia endpoint is "to be determined later." R3 commits to specific snapshot fields (proposals, deadlines, treasury balances, sponsor state) before anyone has confirmed the contracts expose those views.

**The core tension:** The on-chain reader and snapshot formatter — the bulk of the hackathon's novel work — have nothing to read if Sophia's contracts don't exist or expose a different state shape.

**Option A — Verify Sophia before the hackathon and record contract ABIs/addresses.** Promote Sophia from an assumption to a verified prerequisite. Confirm it is live, reachable, and its governance contracts expose the R3 fields.
- Pro: Eliminates the biggest risk to the on-chain reader deliverable.
- Con: If Sophia is not ready, this blocks the hackathon start until a fallback is chosen.

**Option B — Run the demo against anvil + pacto-gov contracts; label Sophia as post-hackathon production target.** Use the already-deployed anvil `pacto-gov` "Nave Pirata" contracts for the hackathon demo. Sophia becomes the production target for a follow-up sprint.
- Pro: Guaranteed working input data; no external dependency.
- Con: The demo runs against a local testnet, which is less compelling than a real chain.

**Option C — Keep Sophia as the target but gate R3's field list on a confirmed ABI read.** Add a pre-hackathon step where A1 confirms each R3 field maps to a callable Sophia contract method. Mark any unverified field as best-effort rather than required.
- Pro: Preserves the Sophia target while honestly bounding the snapshot format.
- Con: If Sophia contracts are not deployed, the gate fails and you fall back to Option B anyway.

**How to decide:** Check whether Sophia is a live chain with deployed `pacto-gov` contracts. If yes, Option A or C. If no, Option B is the safe path. Either way, fix the Assumptions/Q2 contradiction by moving the production Sophia RPC endpoint to an open dependency.

**Resolution:** User clarified the target is **Sepolia** (Ethereum testnet), not "Sophia." Investigation confirmed: `pacto-gov` infrastructure contracts (NavePirataFactory, NavePirataRegistry, master clones, Hats, Safe bundle) are deployed and verified on Sepolia at addresses in `deployments/11155111/full-system.json`. All R3 snapshot fields have corresponding contract read methods. However, zero per-squad governance clones have been bootstrapped on Sepolia yet (`NavePirataRegistry.deploymentCount()` returns 0) — a squad must be deployed via `DeployNavePirata.s.sol` before the bot has real state to read. Local anvil uses the same contracts. Option A chosen with the squad-bootstrapping prerequisite. Requirements doc updated: all "Sophia" references corrected to "Sepolia" with contract addresses, read method mapping, and ABI sourcing notes.

---

## Q3. How should the TEE architecture brief connect to the governance bot work? — RESOLVED

**Decision:** The TEE is a deployment target, not a runtime redesign. The daemon, NIP-46 bunker, and bot handler all run inside the TEE as-is. R15's "no re-architecture" claim is correct — the TEE inherits the entire daemon + handler stack unchanged. The reviewers' concern about a topology change was based on an incorrect assumption that the bot would be extracted from the shared daemon into an isolated single-purpose runtime.

The TEE brief covers: which TEE platforms are appropriate, how to package the stack for each, how sealed storage protects MLS state, how attestation works (`!verify` concept with challenge-response, root of trust, measurement hash), and deployment instructions. No code re-architecture is required.

Requirements doc updated: R13, R14, R15, R16, and the Key Decisions section now reflect this framing.

---

## Q4. Should the demo success criteria distinguish "MLS worked" from "human pasted text"? — RESOLVED

**Decision:** Option C — autonomous posting is the only success bar. The bot must post the snapshot into the Squad channel via the daemon's MLS group send capability with no human intervention. Human-paste fallback is not an acceptable demo outcome.

Requirements doc updated: R7 now requires autonomous posting, R17 requires the bot to join via MLS Welcome and post an encrypted group message with no human intervention, F3 (human-in-the-loop fallback) removed, fallback Key Decision removed, Q5 updated to exclude human-paste fallback.

---

## Q5. How should MLS key custody and group state security be handled? — RESOLVED

**Context:** The doc says key custody "follows the daemon's existing security model (NIP-46 bunker preferred, `nsec` dev-only)." But MLS encryption requires local access to the credential signature key and the key-package init key for Welcome decryption and group-key derivation — these are not Nostr event-signing operations, so a remote NIP-46 bunker cannot service them. MLS credentials also typically use a different signature scheme than the Nostr secp256k1 nsec. Additionally:
- R9 says MLS key material is "never logged or returned in JSON-RPC errors" but says nothing about how it's persisted to disk (existing config is 0o600, but no requirement extends that to MLS group state).
- There is no threat model for what happens if the bot's MLS group state is compromised (the scope defers re-keying, so stolen group state may expose future channel messages until an external re-key).
- The RPC endpoint for on-chain reads has no authentication or integrity requirement — a MITM'd RPC could feed false treasury balances that get signed by the bot.
- The MLS developer dependency is a single point of failure with no named owner or backup plan.

**The core tension:** The doc introduces a second key class (MLS) with no custody story, no at-rest protection requirement, and no threat model, while claiming the existing security model covers it.

**Option A — Add a full MLS security block.** Add requirements for: (a) MLS group state persisted at 0o600/0o700 with at-rest encryption or sealed persistence, (b) MLS credential/init keys held locally under the dev-only regime for the hackathon (production custody sealed inside the TEE per R14), (c) a threat-model statement acknowledging that stolen group state exposes future channel messages until an external re-key, with TEE as the mitigation path, (d) production RPC endpoint must use TLS and validate chain id / block hash checkpoint, (e) name the MLS developer or owning team and add a mid-hackathon checkpoint for cutting to fallback.
- Pro: Complete security posture; implementer has full guidance.
- Con: Adds 5+ requirements, increasing doc weight.

**Option B — Add the minimum: at-rest protection + threat model only.** Add requirements for MLS group state at-rest protection (0o600) and a one-line threat model acknowledging the forward-secrecy gap. Note that MLS key custody (local vs bunker) and RPC integrity are deferred to planning. Keep the MLS developer dependency as-is but add a mid-hackathon checkpoint.
- Pro: Covers the highest-risk gaps without over-loading the doc.
- Con: Leaves key custody and RPC integrity as open questions for planning.

**Option C — Defer all MLS security details to planning.** Note the gaps as assumptions and let planning resolve them.
- Pro: Keeps the requirements doc lean.
- Con: Planning inherits undefined security posture for a crypto subsystem, which is exactly where security gaps hide.

**How to decide:** For a hackathon, Option B is the pragmatic balance — it covers at-rest protection and the threat model (the two things an implementer will get wrong without guidance) while deferring key custody mechanics to planning. Option A is better if this doc will outlive the hackathon as a production reference.

**Resolution:** Investigated mdk-core key material in pacto-app. Option B chosen (minimum: at-rest protection + threat model). Key findings: MLS uses 3 key classes (Ed25519 credential/signature, X25519 leaf encryption, per-epoch derived secrets) — all local-only, NIP-46 bunker cannot service them (curve mismatch). The daemon owns all MLS keys and `vector-mls.db` (unencrypted SQLite, needs 0o600). The handler never touches MLS private material. Requirements doc updated: R9 expanded with full key custody details, R9a added with threat model (forward-secrecy gap + TEE as mitigation + WASM deferred), Scope Boundaries updated with WASM deferral. RPC integrity deferred to planning.

---

## Q6. Where does the snapshot cadence timer and RPC config live — daemon or handler? — RESOLVED

**Context:** R5 says cadence is configurable, and the Sources section places it in `schemas/config.json` (daemon config), implying the daemon owns a timer. But R10 only adds the MLS send method — there is no JSON-RPC method for the daemon to trigger the handler on a cadence, and no requirement describes a scheduler. The daemon's only periodic tasks are metrics (30s) and diagnostics (30s). Meanwhile, `schemas/config.json` has no cadence or RPC fields — it defines only daemon runtime fields and per-bot identity fields. The handler performs the RPC read (F1 step 1), so the cadence timer most naturally lives in the handler. F1 step 2 also introduces an on-chain block cursor/diff that no requirement asks for — every R3 field is a current-state read, and the daemon's existing SQLite cursors are for Nostr events, not on-chain blocks.

**The core tension:** The doc is internally split on where cadence and RPC config live, and F1 introduces a persistence surface (on-chain cursor) that no requirement backs.

**Option A — Handler owns cadence and RPC config; remove from daemon config scope.** State explicitly that the handler owns the cadence timer (external cron/timer calling the handler loop) and the RPC endpoint configuration. Remove the Sources claim about `schemas/config.json` for cadence/RPC. Drop F1 step 2 (the on-chain cursor/diff) since no R3 field consumes it.
- Pro: Simplest; no new daemon subsystem; aligns with how the handler already works.
- Con: Cadence is not configured in the daemon config, which may surprise operators.

**Option B — Daemon owns cadence; add scheduling capability.** Add a new JSON-RPC method and config.json fields for daemon-side scheduling. The daemon triggers the handler on a cadence.
- Pro: Centralized scheduling; operators configure cadence in one place.
- Con: Adds a new daemon subsystem (scheduler) that is not in the hackathon's core scope.

**Option C — Handler owns cadence; daemon config carries RPC endpoint only.** The handler owns the timer, but the RPC endpoint is configured in `pacto-bot-api.toml` and passed to the handler at registration. Drop F1 step 2.
- Pro: Compromise — daemon owns infrastructure config, handler owns scheduling.
- Con: Requires a new config field and a way to pass it to the handler.

**How to decide:** Option A is the most hackathon-appropriate: the handler is a separate process that can run its own timer, and adding a daemon scheduler is scope creep. Drop F1 step 2 regardless — no requirement backs the on-chain cursor.

**Resolution:** Option A — handler owns cadence and RPC config. Requirements doc updated: R5 states the handler owns the cadence timer and RPC endpoint config (not part of daemon config schema), F1 step 2 (on-chain cursor/diff) dropped, F1 steps rewritten as current-state read with no diffing, Sources `schemas/config.json` line corrected to note cadence/RPC are handler-owned.

---

## Q7. How should MLS send be tested in the Docker-free default suite? — RESOLVED

**Context:** R19 requires tests covering the new JSON-RPC method and MLS redaction. The repo's default `cargo test` runs in-process against mock relay and mock bunker in `tests/support/` with no Docker. MLS group send cannot be exercised without a second MLS group member or a mock MLS peer, and neither is mentioned anywhere. An implementer will either skip real MLS-send coverage (leaving R19 partially unmet) or be forced to add an `#[ignore]` Docker-gated test. Additionally, R19 conflates test coverage with `make validate` — but `make validate` runs only `cargo fmt --check` and `cargo clippy`, not the test suite.

**The core tension:** The one genuinely new crypto path (MLS send) has no test strategy, and the test-coverage requirement is conflated with a lint-only command.

**Option A — Add a mock MLS group peer to tests/support/.** A mock MLS peer issues a Welcome and validates an encrypted application message, all in-process. Real-MLS integration is gated behind `#[ignore]` + `PACTO_DEV_ENV=1`. Also fix R19 to name `cargo test` explicitly alongside `make validate`.
- Pro: MLS send is tested in the default suite; R19 is fully met.
- Con: Building a mock MLS peer is non-trivial work for a hackathon.

**Option B — Gate MLS send tests behind #[ignore] + PACTO_DEV_ENV=1 only.** Don't build a mock MLS peer for the hackathon. Test the JSON-RPC method's authorization and error handling without real MLS crypto. Fix R19 to name `cargo test`.
- Pro: Lower test infrastructure burden; still tests the method surface.
- Con: The actual MLS encryption path is untested in the default suite.

**Option C — Note the test gap as a deferred question.** Acknowledge that MLS send testing needs a mock peer and defer the design to planning. Fix R19 to name `cargo test`.
- Pro: Honest; doesn't commit to test infrastructure in the requirements doc.
- Con: Planning inherits an untested crypto path.

**How to decide:** Option B is the hackathon-appropriate balance — test the method surface and authorization without building mock MLS infrastructure. The real MLS crypto path gets `#[ignore]` integration tests. Fix R19 regardless to name `cargo test` separately from `make validate`.

**Resolution:** Investigated the pacto-app smoke test — it's a self-contained, Tauri-free, in-memory MLS round-trip (163 lines, `mls.rs:1578–1740`) that drops into the daemon's test suite almost verbatim. Option A chosen — building a mock MLS group peer is a 2-4 hour task (~100 lines total: ~70 lines of MLS logic adapted from the smoke test + ~30 lines of wrapper). Uses real `mdk-core` with ephemeral in-memory SQLite (`:memory:`), no Docker, no disk, no network. Real-MLS integration tests gated behind `#[ignore]` + `PACTO_DEV_ENV=1`. Requirements doc updated: R19 names `cargo test` separately from `make validate`, references the smoke test as the mock peer source, and describes the test strategy.

---

## Q8. Should the snapshot bot's interaction model be stated explicitly? — RESOLVED

**Context:** The bot only reads public on-chain state and posts — it never receives an event, never responds to a Squad member, and exercises none of the daemon's dispatch/relay/cursor machinery. STRATEGY.md's value proposition is a daemon that multiplexes infrastructure for interactive bots (handlers receive events and reply). A observer of the demo sees a cron-driven Markdown poster, not evidence that `pacto-bot-api` makes bot-building easier. The doc never states whether the one-way, post-only model is intentional or a scope compromise.

**The core tension:** The hackathon demo under-demonstrates the daemon's core value proposition unless the one-way model is explicitly framed as a deliberate v1 choice.

**Option A — Add a one-line interaction model statement.** State that v1 is intentionally one-way: post-only, no replies, no command handling. Confirm this is the intended demo shape, not a scope compromise. If interaction is desired, surface it as a deferred item.
- Pro: Clarifies intent; prevents readers from assuming the bot will be interactive.
- Con: Explicitly bounds the demo's ambition.

**Option B — Add a minimal interactive capability as a stretch goal.** The bot responds to a `!snapshot` command in the channel by posting the current snapshot on demand, in addition to the periodic post. This exercises the daemon's event-receive dispatch path.
- Pro: Demonstrates the daemon's interactive value, not just one-way posting.
- Con: Adds scope — the bot must decrypt incoming messages, which reopens the MLS decryption question.

**Option C — Leave the interaction model implicit.** Don't add a statement; let the requirements speak for themselves.
- Pro: No change needed.
- Con: A reader cannot tell whether the one-way model is intentional or a gap.

**How to decide:** Option A is the lowest-cost fix — one sentence that clarifies intent. Option B is worth considering if you want the demo to prove more of the daemon's value, but it adds meaningful scope (inbound message decryption). Option C leaves the gap unaddressed.

**Resolution:** Option B — add `!snapshot` as a Phase 2 stretch goal. Investigation confirmed it's feasible but not trivial: requires a new kind:445 subscription, live-event dispatch loop with membership filtering, `engine.process_message()` decrypt path, and a `ReceiveGroupMessages` capability (~150-250 lines on top of R8's ~300-line send-only layer). It flips the bot from send-only to bidirectional, expanding the R9a threat model (inbound ratchet processing). Gated on Phase 1 success — if R8 slips, Phase 2 is dropped. Requirements doc updated: R6 updated to note phase boundary, R21-R25 added for Phase 2 stretch goal, F4 flow added, Scope Boundaries updated.
# Handoff Prompt: Brainstorm NostrBotKit Feature Adoption for pacto-bot-api

## Goal

Read `docs/research/nostrbotkit-features-deep-dive.md` and produce a self-contained brainstorm requirements document at `docs/brainstorms/2026-07-01-nostrbotkit-adoption-brainstorm.md` that synthesizes the research into a concrete, prioritizable feature plan for `pacto-bot-api`.

This is a **single-pass, no-interaction** task. Do not ask the user clarifying questions. Make decisions that are consistent with the existing architecture, security model, and planning documents in this repo.

## Required output

The brainstorm document must follow the `ce-brainstorm` skill structure:

1. **Problem Frame** — What problem the NostrBotKit features solve for `pacto-bot-api` operators and developers. Keep it grounded in the research; do not invent new problems.
2. **Requirements** — A numbered list of concrete requirements derived from the research. Each requirement should be testable or demonstrable. Reference existing requirements (R1–R37, KTDs) where they exist.
3. **Key Technical Decisions** — The main architectural choices implied by adopting each feature (e.g., daemon vs CLI responsibility, loopback-only HTTP, JSON-RPC vs built-in command catalog). Include alternatives considered and why one was chosen.
4. **Acceptance Examples** — 3–5 end-to-end scenarios that would prove the brainstormed feature set works. Use the Given/When/Then format.
5. **Scope Boundaries** — What is in scope, what is out of scope, and what is explicitly deferred to later phases (e.g., Phase 2 Marmot, payment gating, web admin UI write operations).
6. **First Slice Recommendation** — A minimal shippable first PR/iteration that delivers the highest-value, lowest-risk feature(s). Include files to touch, methods to add, and tests to write.
7. **Risks & Open Questions** — Security, performance, and dependency risks from the research, plus any questions that cannot be answered from the research alone.

## Constraints

- Do not change code. This is a documentation-only task.
- Preserve the daemon / CLI / handler separation from the architecture plan (`docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`).
- Do not propose giving the daemon identity-creation or deletion capabilities (KTD-8).
- Keep HTTP surfaces loopback-only unless explicitly noted as a future exception.
- Treat secrets (`nsec`, bunker URI, NWC URI, webhook tokens) with the same hygiene as the existing codebase.
- Do not duplicate the research verbatim; synthesize and structure it for implementation planning.

## Sources to read

- `docs/research/nostrbotkit-features-deep-dive.md` (primary)
- `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` (for R1–R37, KTDs, and phase boundaries)
- `docs/pacto-bot-admin-llms.txt` (for operator model context)
- `schemas/jsonrpc.json` (for existing JSON-RPC catalog)

## Output path

`docs/brainstorms/2026-07-01-nostrbotkit-adoption-brainstorm.md`

## Success criteria

- The document is complete, internally consistent, and reads like a requirements doc, not a copy-paste of the research.
- Every feature from the research is addressed with an effort/priority assessment.
- The first slice recommendation is concrete enough that a developer could start implementation from it.
- No TODOs, placeholders, or “to be determined” sections are left unfilled; if something is truly unknown, state the assumption and move on.

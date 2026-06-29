---
title: "feat: Improve pacto-bot-admin help text and add LLM operator's guide"
type: feat
date: 2026-06-29
origin: docs/brainstorms/2026-06-29-admin-cli-help-and-llm-guide-requirements.md
---

## Summary

Improve `pacto-bot-admin` usability by enriching every `--help` screen with examples and flag-value guidance, and add an LLM-readable operator's guide exposed through `--llm-help` and `docs --format llm`. The guide and the CLI help share one source of truth, and a committed `docs/pacto-bot-admin-llms.txt` file is kept in sync by an integration test.

## Problem Frame

Bot operators currently run `pacto-bot-admin --help` and see clap-default one-line descriptions with no examples, no valid-value hints for flags like `--backend` or `--capabilities`, and no pointer to broader operator documentation. They must infer usage from the README or trial and error. LLM users have no single artifact to point a model at for accurate answers. This raises the support burden and slows first-time setup.

## Requirements

### CLI help text

- R1. Every `pacto-bot-admin` subcommand's `--help` output includes at least one concrete example invocation under an "Examples" section.
- R2. Flag help text for `--backend` lists valid values (`nsec`, `bunker_local`, `bunker_remote`) and notes that `nsec` is dev-only.
- R3. Flag help text for `--capabilities` lists valid capability values and explains they are granted to handlers for the new bot.
- R4. Flag help text for `--format` on `diagnose` and `status` lists valid values (`text`, `json`) and their use cases.
- R5. The top-level `--help` output includes a short quick-start example block and a pointer to the LLM guide.

### LLM-friendly operator's guide

- R6. `pacto-bot-admin --llm-help` prints the LLM-readable operator's guide to stdout and exits 0.
- R7. `pacto-bot-admin docs --format llm` prints the same guide to stdout.
- R8. The guide is emitted as Markdown and includes sections: Overview, CLI command reference with examples, Daemon configuration reference, Handler JSON-RPC basics, and When to use which.
- R9. The "When to use which" section explicitly distinguishes: admin CLI for lifecycle and diagnostics, daemon for runtime, JSON-RPC for handler logic.
- R10. The guide uses placeholders for secrets (`<NSEC>`, `<BUNKER_URI>`, `<HTTP_TOKEN>`) and never includes real signing material or tokens.

### Committed artifact and generation

- R11. The guide is committed to the repo at `docs/pacto-bot-admin-llms.txt` so it can be read directly by LLMs without running the CLI.
- R12. A test verifies that `docs/pacto-bot-admin-llms.txt` matches the output of `pacto-bot-admin --llm-help`.

## Key Technical Decisions

- **KTD-1. Guide content lives in the CLI source, not a separate markdown file.** The operator's guide is assembled from static Rust strings plus clap-derived command metadata. This keeps `--help` examples and the LLM guide synchronized; the committed file is a build artifact.
- **KTD-2. Sync verification is an integration test, not only an xtask step.** `cargo test` runs the sync check by default, so CI catches drift without requiring contributors to remember a separate xtask.
- **KTD-3. Use clap's built-in derive features for help text and global flags.** `after_help`, `long_about`, and a global `--llm-help` flag require no custom help renderer and keep the CLI idiomatic.

## Implementation Units

### U1. Enrich clap help text with examples and flag-value guidance

- **Goal:** Every subcommand `--help` has concrete examples and flags explain their valid values.
- **Requirements:** R1, R2, R3, R4, R5
- **Dependencies:** none
- **Files:** `src/admin.rs`, `tests/admin_cli_help.rs`
- **Approach:** Add clap `after_help` and `long_about` attributes to the top-level `Cli` and each `Command` variant. Update the doc comments for `--backend`, `--capabilities`, and `--format` to enumerate valid values and usage notes. Add a top-level `after_help` block with quick-start examples and a pointer to `--llm-help`.
- **Patterns to follow:** Existing clap derive usage in `src/admin.rs:52-133`; existing doc-comment help style in the same file.
- **Test scenarios:**
  - `pacto-bot-admin new --help` contains an example invocation and lists `nsec`, `bunker_local`, and `bunker_remote`.
  - `pacto-bot-admin diagnose --help` lists `text` and `json` for `--format`.
  - Top-level `--help` contains a quick-start example and a pointer to `--llm-help`.
  - No `--help` output contains a real nsec, bunker URI, or HTTP token.
- **Verification:** `cargo test --test admin_cli_help` passes and sample `--help` output is inspected.

### U2. Implement operator's guide generator

- **Goal:** Produce the markdown operator's guide from code so the CLI and committed file share one source of truth.
- **Requirements:** R8, R9, R10
- **Dependencies:** U1
- **Files:** `src/guide.rs` (new), `src/admin.rs`
- **Approach:** Create a `guide` module with a `render_llm_guide() -> String` function. Build markdown from static sections plus clap-derived command metadata. Declare `mod guide;` in `src/admin.rs`. Sections: Overview, CLI command reference with examples, Daemon configuration reference, Handler JSON-RPC basics, and When to use which. Use placeholders for all secrets.
- **Patterns to follow:** Existing structured-output generation in `src/diagnostics.rs`.
- **Test scenarios:**
  - Rendered guide includes all five required sections.
  - Guide contains example commands for every subcommand.
  - Guide contains no literal secret patterns (`nsec1...`, real bunker URIs, hex tokens).
- **Verification:** Unit tests in `src/admin/guide.rs` pass.

### U3. Wire `--llm-help` flag and `docs --format llm` subcommand

- **Goal:** Expose the operator's guide through both requested CLI surfaces.
- **Requirements:** R6, R7
- **Dependencies:** U2
- **Files:** `src/admin.rs`
- **Approach:** Add a global `--llm-help` flag to `Cli` that prints the guide and exits before subcommand dispatch. Add a `Docs { #[arg(short, long)] format: String }` subcommand that prints the guide when `--format llm` is supplied and rejects unknown formats.
- **Patterns to follow:** Global flag pattern already used for `--config` and `--data-dir`; subcommand pattern used for existing commands.
- **Test scenarios:**
  - `pacto-bot-admin --llm-help` prints the guide and exits 0.
  - `pacto-bot-admin docs --format llm` prints identical output.
  - `pacto-bot-admin docs --format unknown` exits non-zero with a clear error.
- **Verification:** Run the CLI commands and compare stdout.

### U4. Add xtask docs and commit generated llms.txt

- **Goal:** Generate the committed operator's guide file from the same source as the CLI.
- **Requirements:** R11
- **Dependencies:** U2, U3
- **Files:** `xtask/src/main.rs`, `xtask/src/docs.rs` (new), `docs/pacto-bot-admin-llms.txt`
- **Approach:** Add a `Docs` subcommand to xtask. It invokes the guide generator (or runs the built binary with `--llm-help`) and writes the output to `docs/pacto-bot-admin-llms.txt`. Check the generated file into git.
- **Patterns to follow:** Workspace-root discovery and file emission in `xtask/src/codegen.rs`.
- **Test scenarios:**
  - `cargo xtask docs` creates or updates `docs/pacto-bot-admin-llms.txt`.
  - Running `cargo xtask docs` twice is idempotent when no source changes.
- **Verification:** Run `cargo xtask docs` and inspect the file.

### U5. Verify llms.txt stays in sync

- **Goal:** Prevent drift between the committed file and the CLI output.
- **Requirements:** R12
- **Dependencies:** U4
- **Files:** `tests/admin_cli_llms_txt_sync.rs`
- **Approach:** Integration test runs the `pacto-bot-admin` binary with `--llm-help`, reads `docs/pacto-bot-admin-llms.txt`, and asserts they match.
- **Patterns to follow:** `assert_cmd` integration test pattern in `tests/admin_cli_creation.rs`.
- **Test scenarios:**
  - File content matches `--llm-help` output.
  - If the file is edited manually without rebuilding, the test fails.
- **Verification:** `cargo test --test admin_cli_llms_txt_sync` passes.

## Scope Boundaries

- Man pages, shell completion scripts, and interactive wizard mode are deferred.
- Actual admin CLI behavior (subcommand semantics, JSON-RPC catalog) is unchanged.
- The LLM guide is a condensed operator reference; it does not replace the full architecture deep dives or implementation plan.

## Risks & Dependencies

- **Sync drift.** If contributors edit help text without regenerating `docs/pacto-bot-admin-llms.txt`, the sync test in U5 fails until they run `cargo xtask docs`.
- **Stale JSON-RPC catalog.** The operator's guide covers Phase 1 handler JSON-RPC basics; later phases may need guide updates as the catalog grows.
- **Dependency.** clap 4 derive features support `after_help`, `long_about`, and global flags (confirmed in `Cargo.toml`).

## Acceptance Examples

- AE1. Covers R1, R2, R5.
  - **Given:** A user runs `pacto-bot-admin new --help`.
  - **Then:** The output shows an example like `pacto-bot-admin new echo-bot --backend nsec --relays ws://localhost:7000` and lists the valid `--capabilities` values.
- AE2. Covers R6, R8, R9, R10.
  - **Given:** A user runs `pacto-bot-admin --llm-help`.
  - **Then:** The output includes a "When to use which" section and uses placeholders such as `<NSEC>` instead of real secrets.
- AE3. Covers R11, R12.
  - **Given:** A contributor changes the CLI help examples but forgets to regenerate the committed file.
  - **Then:** CI fails because `docs/pacto-bot-admin-llms.txt` does not match `pacto-bot-admin --llm-help`.

## Sources / Research

- `docs/brainstorms/2026-06-29-admin-cli-help-and-llm-guide-requirements.md` — origin requirements doc.
- `src/admin.rs:52-133` — current clap CLI definitions.
- `xtask/src/codegen.rs` — existing xtask patterns for workspace-root discovery and file emission.
- `tests/admin_cli_creation.rs` — existing `assert_cmd` integration test patterns.

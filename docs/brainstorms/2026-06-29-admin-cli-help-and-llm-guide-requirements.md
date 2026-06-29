---
date: 2026-06-29
topic: admin-cli-help-and-llm-guide
---

## Summary

Improve `pacto-bot-admin` usability by adding examples and flag-value guidance to every `--help` screen, and provide an LLM-readable operator's guide via both a `--llm-help` flag and a `docs --format llm` subcommand. The guide covers admin CLI workflows, daemon configuration, handler JSON-RPC basics, and when to use each surface.

## Problem Frame

Bot operators currently run `pacto-bot-admin --help` and see clap-default one-line descriptions with no examples, no valid-value hints for flags like `--backend` or `--capabilities`, and no pointer to broader operator documentation. They must infer usage from the README or trial and error. LLM users have no single artifact to point a model at for accurate answers. This raises the support burden and slows first-time setup.

## Key Decisions

- **Examples live in the CLI source.** `--help` and the LLM doc share one source of truth for examples and flag guidance, rather than maintaining a separate docs-first file.
- **Two access paths for the LLM guide.** `--llm-help` is a global convenience flag. `docs --format llm` is the extensible subcommand home for future formats such as `man` or `markdown`.
- **Operator's guide, not just CLI reference.** The LLM doc includes daemon config and handler JSON-RPC basics with explicit "when to use which" guidance, because operators need to understand the full lifecycle surface.

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
- R12. A test or xtask verifies that `docs/pacto-bot-admin-llms.txt` matches the output of `pacto-bot-admin --llm-help`.

## Key Flows

- F1. Operator learns a command
  - **Trigger:** An operator runs `pacto-bot-admin <command> --help`.
  - **Steps:** They read the description, see valid flag values, and follow the printed example.
  - **Outcome:** They can construct a working command without leaving the terminal.
- F2. Operator queries an LLM
  - **Trigger:** An operator wants to know how to migrate a bot or set up a bunker.
  - **Steps:** They either run `pacto-bot-admin --llm-help` or open `docs/pacto-bot-admin-llms.txt` and paste it into their LLM context.
  - **Outcome:** The LLM answers accurately using the canonical operator's guide.
- F3. Contributor updates help content
  - **Trigger:** A developer changes CLI help text or examples.
  - **Steps:** They run `cargo xtask docs` (or equivalent) to regenerate `docs/pacto-bot-admin-llms.txt`; CI validates the file is in sync.
  - **Outcome:** The committed guide never drifts from the CLI source.

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

## Scope Boundaries

- Man pages, shell completion scripts, and interactive wizard mode are deferred.
- Changes to actual admin CLI behavior beyond help text and documentation output are out of scope.
- The LLM guide is a condensed operator reference. It does not replace the full architecture deep dives or the implementation plan.

## Success Criteria

- A first-time operator can run `pacto-bot-admin new --help` and understand how to create a bot without reading other docs.
- `pacto-bot-admin --llm-help` produces a document that an LLM can use to answer common operator questions accurately.
- The committed `docs/pacto-bot-admin-llms.txt` stays in sync with the CLI source.

## Sources / Research

- `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` defines the planned admin CLI surface.
- `src/admin.rs` implements the current clap-based CLI.
- Running `/usr/local/bin/pacto-bot-admin --help` confirmed the current sparse help output lacks examples and flag-value guidance.

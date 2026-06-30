---
date: 2026-06-30
topic: bot-scaffold
---

## Summary

Add a Rust-native scaffold feature to `pacto-bot-admin` that generates a complete, operationally-ready bot project. Two entry points serve different starting states: `pacto-bot-admin new --scaffold <bot-id>` creates a new bot identity and the handler project, while `pacto-bot-admin scaffold <bot-id>` assumes the identity already exists. The first version targets Python using the published `pacto_bot_api` SDK, but the structure is designed so additional languages and future WASM deployment can be added without reworking the CLI.

## Problem Frame

Today a developer who wants to write a Pacto bot must assemble the pieces by hand: copy `examples/echo_bot.py` or `python/examples/greeting_bot.py`, trim it down, write their own `Dockerfile` and `docker-compose.yml`, figure out how to run against a host daemon, and remember to set file permissions on `pacto-bot-api.toml`. The repo has good reference material, but there is no opinionated, runnable starting point. For developers who want the path of least resistance, this hand-assembly is friction before they have written any bot logic.

## Key Decisions

- **Two entry points, one generator.** `new --scaffold` creates the bot identity and scaffolds the project; `scaffold` scaffolds a project for an existing identity. This lets a developer retrofit scaffolding without re-creating keys.
- **Template-directory driven.** The CLI copies and substitutes a per-language template tree rather than building files from inline strings. This keeps templates readable, makes adding languages mechanical, and leaves room for future WASM packaging templates.
- **Full operational scaffold by default.** The generated project includes the handler file, a `Dockerfile`, a root-level `docker-compose.yml`, a systemd unit for host-daemon mode, a `pacto-bot-api.toml`, a `README.md`, and optionally pytest files. The goal is one command to a runnable project.
- **Multi-bot-aware layout.** A project can hold one bot or many. In a multi-bot project, each bot lives under `bots/<bot-id>/` with its own handler and Dockerfile; the compose file and systemd units live at the project root.
- **Python-only in v1, multi-language in design.** The language is required at scaffold time and defaults to Python because that is the only published SDK today. The template layout is not Python-specific.
- **Tests are default-on for new projects, additive for retrofits.** `new --scaffold` generates pytest files by default because first-time developers benefit from an immediate runnable test. `scaffold` only adds tests when `--with-tests` is passed, and re-running with the flag adds test files without overwriting the handler.

## Requirements

### Entry points and identity handling

- R1. `pacto-bot-admin new --scaffold <bot-id>` creates a new bot identity (keypair and `[[bots]]` config entry) and scaffolds a handler project in one run.
- R2. `pacto-bot-admin scaffold <bot-id>` scaffolds a handler project using an existing bot identity from the daemon config; it fails fast if the identity does not exist.
- R3. Both commands accept `--language <lang>` and default to `python` when the language is omitted.
- R4. `new --scaffold` generates pytest files by default and supports `--no-tests` to skip them; `scaffold` only generates tests when `--with-tests` is passed.
- R5. Both commands accept `--commands <list>` to pre-seed slash-command stubs; in interactive mode the user is prompted for command names.
- R6. `new --scaffold` writes the generated `[[bots]]` entry into the project’s `pacto-bot-api.toml` rather than printing it to stdout.

### Generated project layout

- R7. The generated project root contains `pacto-bot-api.toml`, `docker-compose.yml`, `README.md`, and a systemd unit file for host-daemon development.
- R8. In a single-bot project, the handler file and `Dockerfile` live at the project root.
- R9. In a multi-bot project, each bot lives under `bots/<bot-id>/` with its handler file and `Dockerfile`; the root compose file can target any subset of `bots/*`.
- R10. The generated `README.md` explains how to run the bot against a host daemon and how to run the full compose stack.
- R11. The generated `pacto-bot-api.toml` references the scaffolded bot identity with the relays and capabilities collected during the command.

### Docker and operational files

- R12. The generated `Dockerfile` builds a container image for the bot handler using the published Python SDK.
- R13. The generated `docker-compose.yml` supports at least two profiles: `bot-only` (bot talking to a daemon on the host) and `full` (bot, daemon, and bunker).
- R14. The generated systemd unit runs the bot handler against a daemon on the host, using the same transport defaults as the SDK.
- R15. Kubernetes manifests are not generated.

### Generated handler code

- R16. The generated bot uses the published SDK (`from pacto_bot_api import Bot`) and the high-level `@bot.command("/name")` decorator API.
- R17. Each command supplied via `--commands` or interactive prompt becomes a stub handler that returns a placeholder reply.
- R18. The generated bot includes a `@bot.default` handler that ignores unrecognized commands.
- R19. The generated bot accepts the standard SDK CLI flags (`--socket`, `--data-dir`, `--transport`, `--http-bind`, `--secret`) through `bot.run()`.

### Tests

- R20. When `--with-tests` is passed, the CLI generates pytest files that exercise each stub command and the default handler against the `Bot` instance without requiring a live daemon.
- R21. Re-running the scaffold command with `--with-tests` on an existing project adds the test files without overwriting the bot handler, Dockerfile, or config.

### Safety and overwrite behavior

- R22. Before overwriting any existing file, the CLI prompts for permission unless `--force` is passed.
- R23. The CLI refuses to overwrite signing material or a populated `pacto-bot-api.toml` with `--force`; these files must be renamed or removed by the operator.
- R24. The generated `pacto-bot-api.toml` is created with `0o600` permissions or stricter.
- R25. The CLI never writes real `nsec` values into generated handler code, README examples, or test fixtures; placeholders or bunker mode are used instead.

## Key Flows

- F1. First-time developer creates a bot
  - **Trigger:** A developer runs `pacto-bot-admin new --scaffold echo-bot --commands echo,help`.
  - **Actors:** Bot developer, `pacto-bot-admin`, generated project files.
  - **Steps:** The CLI generates a keypair, writes `pacto-bot-api.toml` with the new identity, creates the handler file with `/echo` and `/help` stubs, creates Dockerfile/compose/systemd/README, and optionally generates pytest files.
  - **Outcome:** The developer can `cd echo-bot`, `docker compose --profile full up --build`, and have a running bot.

- F2. Developer retrofits tests into an existing scaffolded bot
  - **Trigger:** A developer runs `pacto-bot-admin scaffold echo-bot --with-tests` in a project that already contains `bots/echo-bot/`.
  - **Actors:** Bot developer, `pacto-bot-admin`.
  - **Steps:** The CLI detects existing files, skips the handler and Dockerfile, writes the pytest files, and reports which files were added.
  - **Outcome:** The project now has tests without losing existing bot logic.

- F3. Operator adds a second bot to an existing project
  - **Trigger:** A developer runs `pacto-bot-admin scaffold price-bot --commands price` inside an existing multi-bot project root.
  - **Actors:** Bot developer, `pacto-bot-admin`.
  - **Steps:** The CLI writes `bots/price-bot/`, updates the root `docker-compose.yml` to include the new service, and appends the new `[[bots]]` entry to `pacto-bot-api.toml`.
  - **Outcome:** One compose file now runs both bots against the shared daemon and bunker.

## Scope Boundaries

### Deferred for later

- Kubernetes manifests and Helm charts.
- Languages other than Python in the first version; the CLI still requires `--language` so the template engine can grow into Rust, Go, TypeScript, etc.
- WASM runtime handler support; the project layout should not block it.
- Contract-test manifest generation like `examples/greeting_bot.manifest.json`.
- Auto-registration of the handler with a running daemon.
- Auto-publishing of the bot profile via `pacto-bot-admin publish-profile`.

### Outside this product's identity

- This feature is about developer onboarding and project scaffolding, not about changing the daemon runtime, the JSON-RPC protocol, or the admin CLI’s identity-management behavior beyond writing config.

## Acceptance Examples

- AE1. Covers R1, R7, R16, R17.
  - **Given:** A user runs `pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo`.
  - **Then:** A directory `echo-bot/` is created containing `pacto-bot-api.toml` (mode `0o600`), `echo_bot.py` with `@bot.command("/echo")`, `Dockerfile`, `docker-compose.yml`, `README.md`, and a systemd unit. The config contains a generated `[[bots]]` entry for `echo-bot`.

- AE2. Covers R2, R22, R23.
  - **Given:** A user runs `pacto-bot-admin scaffold existing-bot` and `bots/existing-bot/` already exists.
  - **Then:** The CLI prompts before overwriting and refuses to overwrite `pacto-bot-api.toml` even if `--force` is passed.

- AE3. Covers R20, R21.
  - **Given:** A user runs `pacto-bot-admin scaffold echo-bot --with-tests` in a project where `echo_bot.py` already exists but no tests exist.
  - **Then:** The CLI adds test files without modifying `echo_bot.py`.

- AE4. Covers R13.
  - **Given:** A user runs `docker compose --profile full up --build` in a freshly scaffolded project.
  - **Then:** Containers for the bot, daemon, and bunker start and the bot connects to the daemon.

## Dependencies / Assumptions

- The published Python SDK in `python/` is installable and stable enough to be the default scaffold target.
- The daemon config format and `pacto-bot-admin new` key generation behavior remain stable; the scaffold feature depends on the same TOML snippet shape.
- Docker and docker compose are available on the developer’s machine for the operational path; the host-daemon path works without them.
- Developers who choose `nsec` backend understand it is dev-only; generated README must repeat this warning.

## Outstanding Questions

- **Resolved:** The CLI appends new services to the root `docker-compose.yml` and leaves hand-edits intact when adding a bot to an existing project.
- **Resolved:** `--with-tests` is default for `new --scaffold` and opt-in for `scaffold`.
- **Deferred to planning:** Exact substitution syntax for template files (e.g., `{{bot_id}}`, `{% if commands %}`).

## Sources / Research

- `src/admin.rs` — current `pacto-bot-admin new` implementation and interactive flow.
- `python/README.md` — published Python SDK quickstart and `Bot` decorator API.
- `examples/greeting_bot.manifest.json` — example contract manifest (deferred for this feature).
- `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` — daemon config schema and admin CLI lifecycle.
- `docs/brainstorms/2026-06-29-admin-cli-help-and-llm-guide-requirements.md` — adjacent admin CLI usability work.

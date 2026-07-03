# Agent Instructions for {{bot_id}} Project

This file guides AI assistants working on the `{{bot_id}}` Pacto bot project.

## Project context

This is a Pacto bot handler project. The Rust daemon `pacto-bot-api` manages
bot identities, Nostr relay connections, and encrypted messaging. The Python
bot handler in `bots/` connects to the daemon over a Unix socket or HTTP using
the `pacto_bot_sdk` SDK.

## Key files

- `pacto-bot-api.toml` — daemon configuration with bot identities, relays, and
  signing backends. Created by `pacto-bot-admin`. Treat as secret; contains or
  references signing material.
- `docker-compose.yml` — local orchestration. Default stack: daemon + bot.
  Use `--profile with-bunker` to add the NIP-46 bunker, `--profile relay` to
  add an internal Nostr relay, or `--profile full` for everything. Set
  `PACTO_RELAY_URL` and `PACTO_BUNKER_URI` to point to internal or external
  services.
- `bots/{{bot_id}}/{{bot_id_snake}}.py` — the bot handler entry point.
- `bots/{{bot_id}}/pyproject.toml` — Python package metadata for the bot. The
  bot depends on the `pacto-bot-sdk` PyPI package.
- `.pacto/bots/{{bot_id}}/scaffold.lock` — records the resolved contract,
  SDK, and template versions used to create this project. Check it into version
  control.

## Working conventions

- Use the `python-pacto-bot` skill before writing or modifying bot code.
- Keep bot logic in `bots/<bot-id>/`. Add new bots with
  `pacto-bot-admin scaffold <bot-id>` rather than hand-creating files.
- Do not edit `pacto-bot-api.toml` signing material by hand; use
  `pacto-bot-admin` for identity operations.
- Never commit real `nsec`, bunker URIs, or daemon secrets to version control.

## When asked to write a bot

1. Read the `python-pacto-bot` skill.
2. Inspect the existing handler in `bots/{{bot_id}}/{{bot_id_snake}}.py` and the
   capabilities in `pacto-bot-api.toml`.
3. Add or edit command handlers using the `Bot` decorator API from the SDK.
4. Run the generated tests in `bots/{{bot_id}}/tests/test_handlers.py` to verify.

## When asked to add a bot

Use `pacto-bot-admin scaffold <bot-id> --commands <cmd1,cmd2>`. If the bot
identity does not exist yet, create it first with `pacto-bot-admin new`.

## When asked to update the project

Run `pacto-bot-admin update` from the project root. The CLI will re-render the
bot from the locked template and show a diff before overwriting non-protected
files. Protected files are skipped unless you pass `--force`.

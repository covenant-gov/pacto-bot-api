# Agent Instructions for {{bot_id}} Project

This file guides AI assistants working on the `{{bot_id}}` Pacto bot project.

## Project context

This is a Pacto bot handler project. The Rust daemon `pacto-bot-api` manages
bot identities, Nostr relay connections, and encrypted messaging. The Python
bot handler in `bots/` connects to the daemon over a Unix socket or HTTP using
the `pacto_bot_api` SDK.

## Key files

- `pacto-bot-api.toml` — daemon configuration with bot identities, relays, and
  signing backends. Created by `pacto-bot-admin`. Treat as secret; contains or
  references signing material.
- `docker-compose.yml` — local orchestration. Profiles: `bot-only` (talks to a
  host daemon) and `full` (starts daemon + bunker + bot).
- `bots/{{bot_id}}/{{bot_id_snake}}.py` — the bot handler entry point.
- `bots/{{bot_id}}/pyproject.toml` — Python package metadata for the bot.
- `sdk/` — vendored Python SDK source and wheel.
- `skills/python-pacto-bot/SKILL.md` — detailed skill for writing Pacto bots.

## Working conventions

- Use the `python-pacto-bot` skill (`skills/python-pacto-bot/SKILL.md`) before
  writing or modifying bot code.
- Keep bot logic in `bots/<bot-id>/`. Add new bots with
  `pacto-bot-admin scaffold <bot-id>` rather than hand-creating files.
- Do not edit `pacto-bot-api.toml` signing material by hand; use
  `pacto-bot-admin` for identity operations.
- Never commit real `nsec`, bunker URIs, or daemon secrets to version control.

## When asked to write a bot

1. Read `skills/python-pacto-bot/SKILL.md`.
2. Inspect the existing handler in `bots/{{bot_id}}/{{bot_id_snake}}.py` and the
   capabilities in `pacto-bot-api.toml`.
3. Add or edit command handlers using the `Bot` decorator API from the SDK.
4. Run the generated tests in `bots/{{bot_id}}/tests/test_bot.py` to verify.

## When asked to add a bot

Use `pacto-bot-admin scaffold <bot-id> --commands <cmd1,cmd2>`. If the bot
identity does not exist yet, create it first with `pacto-bot-admin new`.

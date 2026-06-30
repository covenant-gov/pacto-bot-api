# Agent Instructions for {{bot_id}}

This file guides AI assistants working on the `{{bot_id}}` bot handler.

## Bot overview

`bots/{{bot_id}}/{{bot_id_snake}}.py` is a Pacto bot handler built on the
`pacto_bot_api` SDK. It connects to the `pacto-bot-api` daemon and responds to
incoming events.

## Capabilities

Check `pacto-bot-api.toml` (project root) for the configured capabilities for
this bot. Common capabilities:

- `ReadMessages` — receive decrypted DMs and group messages
- `SendMessages` — send replies as the bot
- `ManageProfile` — update the bot's kind:0 profile

## SDK reference

The `pacto_bot_api` SDK is vendored in `sdk/` at the project root. The full
agent skill is at `skills/python-pacto-bot/SKILL.md`.

## How to modify this bot

1. Read `skills/python-pacto-bot/SKILL.md` for the SDK API and patterns.
2. Open `bots/{{bot_id}}/{{bot_id_snake}}.py`.
3. Use `@bot.command("/name")` to add slash commands or edit existing handlers.
4. Keep `@bot.default` for unrecognized commands.
5. Run tests:
   ```bash
   cd bots/{{bot_id}}
   pytest
   ```

## How to add commands

Add a new handler function:

```python
@bot.command("/price")
async def price(event, bot):
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": "price placeholder response",
    }
```

After adding commands, update the tests in `bots/{{bot_id}}/tests/test_bot.py`
if necessary.

## Packaging and deployment

- `Dockerfile` — builds a container image for this bot.
- `systemd.service` — runs the bot as a non-root `pacto-bot` service on the host.
- `README.md` — human-facing run and deploy instructions for this bot.

Do not delete or rename these files without updating the project-level
`docker-compose.yml` and `pacto-bot-api.toml`.

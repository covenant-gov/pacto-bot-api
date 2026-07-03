#!/usr/bin/env python3
"""Greeting bot using the generated Pacto Python SDK.

Responds to ``/hello`` with a friendly welcome message and ignores anything
else. Demonstrates the high-level ``Bot`` decorator API and the canonical
handler response shape.

Capabilities required:
    - ReadMessages
    - SendMessages

Usage:
    python python/examples/greeting_bot.py
    python python/examples/greeting_bot.py --socket /run/pacto-bot-api.sock
    python python/examples/greeting_bot.py --transport http --secret "$PACTO_SECRET_TOKEN"
"""

from __future__ import annotations

from pacto_bot_sdk import Bot

bot = Bot(bot_id="greeting-bot")


@bot.command("/hello")
async def hello(event, bot):
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": "Hello there! Welcome to Pacto.",
    }


@bot.default
async def unknown(event, bot):
    return {"event_id": event.event_id, "action": "ignore"}


if __name__ == "__main__":
    bot.run()

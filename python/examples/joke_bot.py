#!/usr/bin/env python3
"""Joke bot using the generated Pacto Python SDK.

Demonstrates the ``defer`` handler action and a proactive ``agent.send_dm``
 call that delivers the punchline after the handler has already acknowledged
the event.

Capabilities required:
    - ReadMessages
    - SendMessages

Usage:
    python python/examples/joke_bot.py
    python python/examples/joke_bot.py --socket /run/pacto-bot-api.sock
"""

from __future__ import annotations

import asyncio

from pacto_bot_sdk import Bot

bot = Bot(bot_id="joke-bot")


async def _deliver_punchline(bot: Bot, event) -> None:
    """Send the punchline asynchronously after deferring the event."""
    try:
        await asyncio.sleep(0.1)
        await bot.send_dm(
            recipient=event.author,
            content="Because they can't C#!",
        )
    except Exception as exc:  # pragma: no cover - best-effort proactive send
        bot._log(f"failed to deliver punchline: {exc}")


@bot.command("/joke")
async def joke(event, bot):
    """Tell a deferred joke."""
    asyncio.create_task(_deliver_punchline(bot, event))
    return {"event_id": event.event_id, "action": "defer"}


@bot.default
async def unknown(event, bot):
    return {"event_id": event.event_id, "action": "ignore"}


if __name__ == "__main__":
    bot.run()

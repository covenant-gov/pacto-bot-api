"""Bot handler for {{bot_id}}."""

from __future__ import annotations

from pacto_bot_api import Bot

bot = Bot(bot_id="{{bot_id}}")

{% for command in commands %}
@bot.command("/{{command}}")
async def {{command}}_handler(event, bot):
    return {
        "event_id": event.event_id,
        "action": "reply",
        "content": "{{command}} placeholder response",
    }

{% endfor %}

@bot.default
async def unknown(event, bot):
    return {"event_id": event.event_id, "action": "ignore"}


if __name__ == "__main__":
    bot.run()

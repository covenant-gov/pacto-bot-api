#!/usr/bin/env python3
"""Greeting bot using the pacto_sdk seed.

Responds to ``/hello`` with a friendly message and ignores anything else.

Usage:
    python greeting_bot.py
    python greeting_bot.py --socket /run/pacto-bot-api.sock
    python greeting_bot.py --transport http --secret "$PACTO_SECRET_TOKEN"
"""

from __future__ import annotations

import argparse
import asyncio
import sys

from pacto_sdk import PactoClient, add_sdk_arguments


async def hello(event: dict, client: PactoClient) -> dict:
    return client.reply(event["event_id"], "Hello there! Welcome to Pacto.")


async def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description="Greeting bot for pacto-bot-api.")
    add_sdk_arguments(parser)
    args = parser.parse_args(argv)

    client = PactoClient(
        bot_id=args.bot_id,
        socket_path=args.socket,
        data_dir=args.data_dir,
        transport=args.transport,
        secret=args.secret,
        http_bind=args.http_bind,
    )
    client.on("/hello", hello)
    client.on_default(lambda event, client: client.ignore(event["event_id"]))
    await client.run()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)

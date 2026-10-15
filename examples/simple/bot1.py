"""A polling bot that takes its updates from tgin instead of Telegram."""

import asyncio
import logging
import os
import sys

from aiogram import Bot, Dispatcher
from aiogram.client.session.aiohttp import AiohttpSession
from aiogram.client.telegram import TelegramAPIServer
from aiogram.types import Message

TOKEN = os.environ["TOKEN"]
# tgin's pull endpoint, e.g. http://tgin:8080/c/bot1/getUpdates.
TGIN_ENDPOINT = os.environ["TGIN_ENDPOINT"]
TELEGRAM_API_URL = os.getenv("TELEGRAM_API_URL", "https://api.telegram.org")
REPLY = os.getenv("REPLY", "hello from bot 1")


class TginServer(TelegramAPIServer):
    """getUpdates goes to tgin; every other method goes to the Bot API."""

    def api_url(self, token: str, method: str) -> str:
        if method == "getUpdates":
            return TGIN_ENDPOINT
        return super().api_url(token, method)


dp = Dispatcher()


@dp.message()
async def echo_handler(message: Message) -> None:
    await message.answer(REPLY)


async def main() -> None:
    server = TginServer.from_base(TELEGRAM_API_URL)
    bot = Bot(token=TOKEN, session=AiohttpSession(api=server))
    await dp.start_polling(bot)


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, stream=sys.stdout)
    asyncio.run(main())

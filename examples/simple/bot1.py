"""A polling bot whose Bot API is tgin's gateway."""

import asyncio
import logging
import os
import sys

from aiogram import Bot, Dispatcher
from aiogram.client.session.aiohttp import AiohttpSession
from aiogram.client.telegram import TelegramAPIServer
from aiogram.types import Message

TOKEN = os.environ["TOKEN"]
# The gateway path of this bot's pull consumer, e.g. http://tgin:8080/c/bot1:
# getUpdates there polls the consumer, and every other call goes to Telegram
# through tgin.
TGIN_BASE = os.environ["TGIN_BASE"]
REPLY = os.getenv("REPLY", "hello from bot 1")

dp = Dispatcher()


@dp.message()
async def echo_handler(message: Message) -> None:
    await message.answer(REPLY)


async def main() -> None:
    bot = Bot(token=TOKEN, session=AiohttpSession(api=TelegramAPIServer.from_base(TGIN_BASE)))
    await dp.start_polling(bot)


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, stream=sys.stdout)
    asyncio.run(main())

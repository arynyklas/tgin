"""A webhook bot: tgin pushes updates to it."""

import logging
import os
import sys

from aiogram import Bot, Dispatcher
from aiogram.client.session.aiohttp import AiohttpSession
from aiogram.client.telegram import TelegramAPIServer
from aiogram.types import Message
from aiogram.webhook.aiohttp_server import SimpleRequestHandler, setup_application
from aiohttp import web

TOKEN = os.environ["TOKEN"]
TELEGRAM_API_URL = os.getenv("TELEGRAM_API_URL", "https://api.telegram.org")
PORT = int(os.getenv("PORT", "3001"))

dp = Dispatcher()
bot = Bot(token=TOKEN, session=AiohttpSession(api=TelegramAPIServer.from_base(TELEGRAM_API_URL)))


@dp.message()
async def echo_handler(message: Message) -> None:
    await message.answer("hello from bot 3 (webhook)")


def main() -> None:
    logging.basicConfig(level=logging.INFO, stream=sys.stdout)
    app = web.Application()
    SimpleRequestHandler(dispatcher=dp, bot=bot).register(app, path="/bot")
    setup_application(app, dp, bot=bot)
    web.run_app(app, host="0.0.0.0", port=PORT)


if __name__ == "__main__":
    main()

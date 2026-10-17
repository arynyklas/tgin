"""A webhook bot: tgin pushes updates to it, and it answers with webhook replies."""

import logging
import os
import sys

from aiogram import Bot, Dispatcher
from aiogram.client.session.aiohttp import AiohttpSession
from aiogram.client.telegram import TelegramAPIServer
from aiogram.methods import SendMessage
from aiogram.types import Message
from aiogram.webhook.aiohttp_server import SimpleRequestHandler, setup_application
from aiohttp import web

TOKEN = os.environ["TOKEN"]
# The gateway path of this bot's push consumer, e.g. http://tgin:8080/c/bot3:
# calls the bot makes itself go to Telegram through tgin.
TGIN_BASE = os.environ["TGIN_BASE"]
PORT = int(os.getenv("PORT", "3001"))

dp = Dispatcher()
bot = Bot(token=TOKEN, session=AiohttpSession(api=TelegramAPIServer.from_base(TGIN_BASE)))


@dp.message()
async def echo_handler(message: Message) -> SendMessage:
    # Returned, not awaited: aiogram puts the call in its answer to tgin's
    # push (a webhook reply), and tgin makes it.
    return message.answer("hello from bot 3 (webhook)")


def main() -> None:
    logging.basicConfig(level=logging.INFO, stream=sys.stdout)
    app = web.Application()
    # handle_in_background=False: the answer waits for the handler, so it
    # can carry the handler's call.
    SimpleRequestHandler(dispatcher=dp, bot=bot, handle_in_background=False).register(app, path="/bot")
    setup_application(app, dp, bot=bot)
    web.run_app(app, host="0.0.0.0", port=PORT)


if __name__ == "__main__":
    main()

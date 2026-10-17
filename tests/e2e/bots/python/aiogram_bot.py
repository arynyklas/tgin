"""aiogram 3 bot of the end-to-end suite, run through tgin's Bot API gateway.

Environment:
  BOT_TOKEN    the bot token (a fake one; the suite's Telegram is the fake)
  TGIN_BASE    the consumer-scoped gateway base, http://tgin:8080/c/<consumer>
  MODE         "poll" (long polling) or "hook" (webhook)
  HOOK_URL     hook mode: the URL registered with setWebhook
  HOOK_SECRET  hook mode: the secret_token registered and checked

Polling: echoes text with sendMessage and sends a document back
(getFile, download through the gateway's file route, sendDocument).
Webhook (handle_in_background=False): answers text with the sendMessage
as the webhook reply (aiogram writes it as multipart/form-data); "getme"
sends a control message over HTTP, then replies with getMe, which the
gateway must refuse.
"""

import logging
import os

from aiogram import Bot, Dispatcher, F, Router
from aiogram.client.session.aiohttp import AiohttpSession
from aiogram.client.telegram import TelegramAPIServer
from aiogram.methods import GetMe, SendMessage
from aiogram.types import BufferedInputFile, Message

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")

MODE = os.environ["MODE"]
router = Router()


@router.message(F.document)
async def document(message: Message, bot: Bot) -> None:
    data = await bot.download(message.document.file_id)
    name = "echo-" + (message.document.file_name or "file")
    await bot.send_document(message.chat.id, BufferedInputFile(data.read(), filename=name))


@router.message(F.text == "getme")
async def getme(message: Message, bot: Bot):
    await bot.send_message(message.chat.id, "getme-control")
    return GetMe()


@router.message(F.text)
async def echo(message: Message, bot: Bot):
    if MODE == "hook":
        # A method object, not a call: it becomes the webhook reply.
        return SendMessage(chat_id=message.chat.id, text=message.text)
    await bot.send_message(message.chat.id, message.text)
    return None


def main() -> None:
    session = AiohttpSession(api=TelegramAPIServer.from_base(os.environ["TGIN_BASE"]))
    bot = Bot(os.environ["BOT_TOKEN"], session=session)
    dp = Dispatcher()
    dp.include_router(router)
    if MODE == "poll":
        # Handles SIGTERM and SIGINT.
        dp.run_polling(bot)
        return

    from aiohttp import web
    from aiogram.webhook.aiohttp_server import SimpleRequestHandler, setup_application

    secret = os.environ["HOOK_SECRET"]

    @web.middleware
    async def guard(request: web.Request, handler):
        # tgin marks its pushes with `Via: 1.1 tgin-<pseudonym>` (RFC 9110).
        if not request.headers.get("Via", "").startswith("1.1 tgin-"):
            return web.Response(status=400, text="no tgin Via header")
        # Checked here too, so the suite does not rest on aiogram's own check.
        if request.headers.get("X-Telegram-Bot-Api-Secret-Token") != secret:
            return web.Response(status=401, text="wrong secret token")
        return await handler(request)

    async def on_startup(bot: Bot) -> None:
        await bot.set_webhook(os.environ["HOOK_URL"], secret_token=secret)

    dp.startup.register(on_startup)
    app = web.Application(middlewares=[guard])
    # aiogram checks the secret as well (401).
    SimpleRequestHandler(
        dispatcher=dp, bot=bot, handle_in_background=False, secret_token=secret
    ).register(app, path="/hook")
    setup_application(app, dp, bot=bot)
    web.run_app(app, host="0.0.0.0", port=8081)


if __name__ == "__main__":
    main()

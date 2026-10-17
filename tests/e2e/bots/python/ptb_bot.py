"""python-telegram-bot 22 bot of the end-to-end suite, run through tgin's
Bot API gateway.

Environment:
  BOT_TOKEN    the bot token (a fake one; the suite's Telegram is the fake)
  TGIN_BASE    the consumer-scoped gateway base, http://tgin:8080/c/<consumer>
  MODE         "poll" (long polling) or "hook" (webhook)
  HOOK_URL     hook mode: the URL registered with setWebhook
  HOOK_SECRET  hook mode: the secret_token registered and checked

Text is echoed with sendMessage; a document is fetched (getFile, then a
download through the gateway's file route, which PTB percent-encodes:
the token's `:` arrives as `%3A`) and sent back with sendDocument. PTB has
no webhook replies: in hook mode it answers over HTTP too.
"""

import logging
import os

from telegram import Update
from telegram.ext import Application, ContextTypes, MessageHandler, filters

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
# httpx logs every request URL, which holds the token.
logging.getLogger("httpx").setLevel(logging.WARNING)
log = logging.getLogger("ptb_bot")


async def echo(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    message = update.effective_message
    await context.bot.send_message(chat_id=message.chat_id, text=message.text)


async def document(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    message = update.effective_message
    file = await context.bot.get_file(message.document.file_id)
    # The URL download_as_bytearray requests; logged without the token.
    encoded = file._get_encoded_url()
    log.info("file download path encodes the token's colon as %%3A: %s", "%3A" in encoded)
    data = await file.download_as_bytearray()
    name = "echo-" + (message.document.file_name or "file")
    await context.bot.send_document(chat_id=message.chat_id, document=bytes(data), filename=name)


def main() -> None:
    base = os.environ["TGIN_BASE"]
    app = (
        Application.builder()
        .token(os.environ["BOT_TOKEN"])
        .base_url(base + "/bot")
        .base_file_url(base + "/file/bot")
        .build()
    )
    app.add_handler(MessageHandler(filters.Document.ALL, document))
    app.add_handler(MessageHandler(filters.TEXT, echo))
    # Both stop on SIGTERM and SIGINT.
    if os.environ["MODE"] == "poll":
        app.run_polling()
    else:
        # PTB checks the secret: a missing or wrong header gets 403.
        app.run_webhook(
            listen="0.0.0.0",
            port=8081,
            url_path="hook",
            webhook_url=os.environ["HOOK_URL"],
            secret_token=os.environ["HOOK_SECRET"],
        )


if __name__ == "__main__":
    main()

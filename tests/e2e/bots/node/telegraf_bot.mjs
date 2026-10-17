// Telegraf bot of the end-to-end suite, run through tgin's Bot API gateway.
//
// Environment:
//   BOT_TOKEN    the bot token (a fake one; the suite's Telegram is the fake)
//   TGIN_BASE    the consumer-scoped gateway base, http://tgin:8080/c/<consumer>
//                (Telegraf resolves methods relative to it, so a trailing slash
//                is added here)
//   MODE         "poll" (long polling) or "hook" (webhook)
//   HOOK_SECRET  hook mode: the secret_token checked
//
// Polling: echoes text with sendMessage, and a document is fetched
// (getFileLink, under the same prefix) and sent back with sendDocument.
// Telegraf waits for a batch's handlers before the next getUpdates, so a text
// starting with "before restart" is held for HOLD_MS after its echo: a stop
// then lands before the next poll, and the shutdown confirm is the only ack.
// Webhook: tgin pushes to a configured endpoint (Telegraf would register an
// https URL); text gets sendChatAction as the webhook reply (on Telegraf's
// allowlist) and the echo over HTTP (sendMessage is not on it).

import { createServer } from "node:http";
import { Telegraf } from "telegraf";
import { message } from "telegraf/filters";

const hook = process.env.MODE === "hook";
const HOLD_MS = 3000;
const bot = new Telegraf(process.env.BOT_TOKEN, {
  telegram: { apiRoot: `${process.env.TGIN_BASE}/`, webhookReply: hook },
});

bot.on(message("text"), async (ctx) => {
  if (hook) await ctx.sendChatAction("typing");
  await ctx.telegram.sendMessage(ctx.chat.id, ctx.message.text);
  if (ctx.message.text.startsWith("before restart")) {
    await new Promise((resolve) => setTimeout(resolve, HOLD_MS));
  }
});

bot.on(message("document"), async (ctx) => {
  const link = await ctx.telegram.getFileLink(ctx.message.document.file_id);
  const response = await fetch(link);
  if (!response.ok) throw new Error(`download failed: ${response.status}`);
  const source = Buffer.from(await response.arrayBuffer());
  const filename = `echo-${ctx.message.document.file_name ?? "file"}`;
  await ctx.telegram.sendDocument(ctx.chat.id, { source, filename });
});

if (hook) {
  const secret = process.env.HOOK_SECRET;
  // Telegraf checks the secret as well (403).
  const handle = bot.webhookCallback("/hook", { secretToken: secret });
  const server = createServer((req, res) => {
    // tgin marks its pushes with `Via: 1.1 tgin-<pseudonym>` (RFC 9110).
    if (!(req.headers.via ?? "").startsWith("1.1 tgin-")) {
      res.statusCode = 400;
      res.end();
      return;
    }
    // Checked here too, so the suite does not rest on Telegraf's own check.
    if (req.headers["x-telegram-bot-api-secret-token"] !== secret) {
      res.statusCode = 401;
      res.end();
      return;
    }
    handle(req, res);
  });
  server.listen(8081, "0.0.0.0", () => console.log("webhook listening"));
  const stop = () => server.close(() => process.exit(0));
  process.once("SIGTERM", stop);
  process.once("SIGINT", stop);
} else {
  // bot.stop() confirms the last offset with getUpdates {offset, limit: 1}.
  process.once("SIGTERM", () => bot.stop("SIGTERM"));
  process.once("SIGINT", () => bot.stop("SIGINT"));
  await bot.launch(() => console.log("polling"));
}

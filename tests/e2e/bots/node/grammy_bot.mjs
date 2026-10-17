// grammY bot of the end-to-end suite, run through tgin's Bot API gateway.
//
// Environment:
//   BOT_TOKEN    the bot token (a fake one; the suite's Telegram is the fake)
//   TGIN_BASE    the consumer-scoped gateway base, http://tgin:8080/c/<consumer>
//                (grammY refuses a trailing slash)
//   MODE         "poll" (long polling) or "hook" (webhook)
//   HOOK_URL     hook mode: the URL registered with setWebhook
//   HOOK_SECRET  hook mode: the secret_token registered and checked
//
// Polling: echoes text with sendMessage, "burst" sends five messages to the
// chat at once, and a document is fetched (getFile, then a download from the
// gateway's file route, which grammY's core leaves to the bot) and sent back
// with sendDocument. Webhook: text is answered with the sendMessage as the
// webhook reply (canUseWebhookReply), which grammY writes as JSON.

import { createServer } from "node:http";
import { Bot, InputFile, webhookCallback } from "grammy";

const token = process.env.BOT_TOKEN;
const base = process.env.TGIN_BASE;
const hook = process.env.MODE === "hook";

const bot = new Bot(token, {
  client: {
    apiRoot: base,
    canUseWebhookReply: (method) => hook && method === "sendMessage",
  },
});

bot.on("message:text", async (ctx) => {
  const chat = ctx.chat.id;
  if (ctx.message.text === "burst") {
    const sends = [1, 2, 3, 4, 5].map((i) => ctx.api.sendMessage(chat, `burst ${i}`));
    await Promise.all(sends);
    return;
  }
  await ctx.api.sendMessage(chat, ctx.message.text);
});

bot.on("message:document", async (ctx) => {
  const file = await ctx.getFile();
  const response = await fetch(`${base}/file/bot${token}/${file.file_path}`);
  if (!response.ok) throw new Error(`download failed: ${response.status}`);
  const data = new Uint8Array(await response.arrayBuffer());
  const name = `echo-${ctx.message.document.file_name ?? "file"}`;
  await ctx.api.sendDocument(ctx.chat.id, new InputFile(data, name));
});

if (hook) {
  const secret = process.env.HOOK_SECRET;
  // grammY checks the secret: a missing or wrong header gets 401.
  const handle = webhookCallback(bot, "http", { secretToken: secret });
  const server = createServer((req, res) => {
    // tgin marks its pushes with `Via: 1.1 tgin-<pseudonym>` (RFC 9110).
    if (req.url !== "/hook" || !(req.headers.via ?? "").startsWith("1.1 tgin-")) {
      res.statusCode = 400;
      res.end();
      return;
    }
    handle(req, res).catch((error) => {
      console.error("webhook handling failed", error);
      if (!res.writableEnded) {
        res.statusCode = 500;
        res.end();
      }
    });
  });
  server.listen(8081, "0.0.0.0", async () => {
    await bot.api.setWebhook(process.env.HOOK_URL, { secret_token: secret });
    console.log("webhook registered");
  });
  const stop = () => server.close(() => process.exit(0));
  process.once("SIGTERM", stop);
  process.once("SIGINT", stop);
} else {
  // bot.stop() confirms the last offset with getUpdates {offset, limit: 1}.
  process.once("SIGTERM", () => bot.stop());
  process.once("SIGINT", () => bot.stop());
  await bot.start({ onStart: () => console.log("polling") });
}

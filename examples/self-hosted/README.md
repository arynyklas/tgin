# tgin with a self-hosted Bot API server

tgin in front of your own [telegram-bot-api](https://github.com/tdlib/telegram-bot-api)
server instead of `api.telegram.org`, with one polling bot behind tgin:

```
           upstream network                bots network
Telegram ── telegram-bot-api ◄── tgin ◄──────────────── bot
              (no published port)  long poll   gateway http://tgin:8080/c/bot
```

- `telegram-bot-api` is built from `docker/telegram-bot-api` (flags, volumes
  and security notes in its README). It runs in local mode, shares the
  `upstream` network with tgin only, and publishes no port.
- tgin long-polls the server (`ingress: long_poll`) and queues the updates
  for the `bot` consumer. Its gateway is the bot's Bot API: the bot's calls
  reach the server through tgin.
- The bot (`examples/simple/bot1.py`) is on the `bots` network with tgin, so
  it cannot reach the server around tgin. It answers every message.
- The server keeps its state (the bots' TDLib databases, the update queue)
  in the `server-state` volume, and the files in `server-files`, which is
  mounted read-only into tgin at the same path. tgin runs as the server's
  user, 10001, because the server's directories are private to that user.

## Run it

You need an api_id and api_hash from <https://my.telegram.org> and a bot
token. Put them in a `.env` file next to `compose.yaml` (it is git-ignored):

```sh
BOT_TOKEN=123456789:...
TELEGRAM_API_ID=...
TELEGRAM_API_HASH=...
TGIN_ADMIN_TOKEN=...   # any long random string
```

A bot that has used `api.telegram.org` must log out of it first: while it
is logged in on two servers, neither is sure to get all of its updates.
Once logged out, it cannot log back in to `api.telegram.org` for 10
minutes, and its pending updates there are dropped. Call `logOut` there
once, directly (tgin's gateway does not forward it):

```sh
curl "https://api.telegram.org/bot$BOT_TOKEN/logOut"
```

Then, from this directory:

```sh
docker compose up -d --build
docker compose logs -f
```

The first build of the server compiles TDLib and takes a while.

## Try it without Telegram

`compose.fake.yaml` swaps the server for tgin's fake Bot API server
(`tests/e2e/fake-telegram`) under the same name, and gives tgin and the bot
the fake token `123456:self_hosted_example_fake_token`. `fake.env` only
satisfies the variables `compose.yaml` requires; a token in your shell's
environment is not used. Nothing reaches Telegram.

```sh
docker compose --env-file fake.env -f compose.yaml -f compose.fake.yaml up -d --build --wait
docker compose --env-file fake.env -f compose.yaml -f compose.fake.yaml port telegram-bot-api 8081
# 127.0.0.1:PORT, the fake's control API (the fake only; the real server is never published)

# A user's message to the bot, queued on the fake as Telegram would:
curl -s -X POST "http://127.0.0.1:PORT/fake/updates/123456:self_hosted_example_fake_token" \
  -H 'Content-Type: application/json' \
  -d '{"message":{"message_id":1,"date":1767225600,"chat":{"id":42,"type":"private","first_name":"Ann"},"from":{"id":42,"is_bot":false,"first_name":"Ann"},"text":"hi"}}'

# tgin polled it, the bot got it from tgin and answered through tgin's gateway:
curl -s "http://127.0.0.1:PORT/fake/calls?method=sendMessage"

docker compose --env-file fake.env -f compose.yaml -f compose.fake.yaml down -v --rmi local
```

The last call lists one `sendMessage` to chat 42 with the text "hello
through a self-hosted Bot API server".

## Deployment rules

- **Never expose the server.** It has no authentication beyond the bot token
  in the URL and no TLS. Anyone who reaches it with a token controls that
  bot; anyone who reaches it with any valid token makes it log in as that
  bot with your api_id. Keep it on a network only tgin can reach, publish
  no port, and do not put bots on that network.
- **Never expose its statistics port.** Leave `--http-stat-port` off; when
  on, the page is unauthenticated, lists bots by token prefix and changes
  the log verbosity on request.
- **One server per token set.** A bot is logged in on exactly one server,
  which owns its update queue: two servers for one token each get part of
  its updates. Many bots may share one server; split them across servers
  with `--filter` if you must.
- **Never one server per tgin replica.** Replicas of tgin share the one
  server their bots live on; a server per replica logs each bot in several
  times.
- **Local mode trusts its callers.** A `file:` path in a request makes the
  server upload any file it can read, other bots' databases included, and
  webhooks may target private addresses. Only tgin reaches the server here.
  At this revision tgin forwards such requests from the bot unchanged, so
  run only bots you trust behind it.
- **Keep the `server-state` volume.** It holds the bots' Telegram sessions
  and their pending updates; losing it logs every bot out of this server.
- **Keep the api_id, api_hash and tokens out of images and version
  control.** Pass them through the environment, as here.

## Files

The server stores files under `/var/lib/telegram-bot-api/files`, and tgin
has that volume read-only at the same path. With `api_url` alone tgin does
not serve it yet: `getFile` answers the server's absolute path, and a
download through tgin's `/file/bot…` route gets the server's 404, as the
server has no such route.

# A Telethon adapter for tgin's MTProto endpoints

`tgin_telethon.py` lets a [Telethon](https://docs.telethon.dev/) program use a user account that tgin runs ([User accounts (MTProto)](../../DOCS.md#user-accounts-mtproto)):

- `Tgin.invoke(request)` sends a Telethon request object to `POST /mtproto/v1/{account}/invoke` as raw TL, with the client's bearer token and `X-Tgin-Layer`, and parses the answer with Telethon. tgin's own refusals and Telegram's `rpc_error` raise `TginError`, with `Retry-After` when there is one.
- `poll(...)` long-polls a pull consumer at `/c/{consumer}/mtproto/v1/getUpdates` and yields each envelope with its `tl`, `users` and `chats` parsed by Telethon's `BinaryReader`. Each poll's `offset` acknowledges what the previous one returned.

It uses Telethon for TL only: no Telethon client, session or connection is created, and the account's authorization stays with tgin.

```sh
uv sync
uv run python
```

```python
from telethon.tl import functions, types
from tgin_telethon import Tgin, poll

tgin = Tgin("http://127.0.0.1:8080", "main", "<the rpc client's token>", layer=230)
tgin.invoke(functions.messages.SendMessageRequest(peer=types.InputPeerSelf(), message="hi"))

for envelope, update, users, chats in poll(tgin, "main-pull", "<the consumer's auth_token>", "worker-1"):
    print(envelope["update_id"], type(update).__name__)
```

The account must be logged in first (`tgin admin login main`, which needs `admin.token` in the config). The config needs an RPC client whose `allow` lists the methods it calls (`messages.sendMessage` above), and a pull consumer with an `auth_token` that the account routes to:

```yaml
accounts:
  main:
    api_id: ${env:TG_API_ID}
    api_hash: ${env:TG_API_HASH}
    session_dir: ./sessions/main
    rpc:
      clients:
        telethon:
          token: ${file:/run/secrets/telethon_token}
          allow: [messages.sendMessage]
    routes:
      - to: [main-pull]
consumers:
  main-pull:
    pull:
      auth_token: ${file:/run/secrets/main_pull_token}
```

## Layers

tgin serves one layer per account (230 with this tgin), and `layer` must be that number: tgin answers `409 layer_mismatch` to any other. Telethon 1.45, pinned in `uv.lock`, serializes in its own layer, 229. Most constructors are the same in both, but about 90 of layer 230's are not in Telethon's, and some changed. A request whose bytes are not valid layer-230 TL is refused with `400 bad_request`, and an update or answer Telethon does not know raises Telethon's `TypeNotFoundError`. Use the adapter for methods and objects the two layers share, check the envelope's `layer`, and move to a Telethon release of the account's layer when there is one.

The adapter is a starting point: it is synchronous, and it leaves retries to you. A `502 no_answer` means the call may have run, so do not resend it blindly.

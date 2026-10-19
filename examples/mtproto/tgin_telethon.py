"""A thin Telethon adapter for tgin's MTProto endpoints.

Calls go to `POST /mtproto/v1/{account}/invoke` as raw TL; updates come from
a pull consumer at `/c/{consumer}/mtproto/v1/getUpdates`, as envelope v1,
whose base64 TL Telethon's `BinaryReader` parses. Telethon serializes in its
own layer, so use it only for objects that layer shares with the account's
(the envelope's `layer`); see README.md.
"""

import base64
import json
import logging
import struct
import time
import urllib.error
import urllib.parse
import urllib.request

from telethon.errors import TypeNotFoundError
from telethon.extensions import BinaryReader

RPC_ERROR = 0x2144CA19
log = logging.getLogger("tgin_telethon")


class TginError(Exception):
    """tgin's own refusal ({"error","description"}) or Telegram's rpc_error."""

    def __init__(self, status, error, description="", retry_after=None):
        super().__init__(f"{status} {error}: {description}")
        self.status, self.error, self.retry_after = status, error, retry_after


class Tgin:
    def __init__(self, base_url, account, token, layer, attempts=3):
        self.base, self.account, self.token, self.layer = base_url.rstrip("/"), account, token, layer
        self.attempts = attempts

    def invoke_url(self):
        return f"{self.base}/mtproto/v1/{urllib.parse.quote(self.account, safe='')}/invoke"

    def updates_url(self, consumer, offset, timeout):
        query = urllib.parse.urlencode({"offset": offset, "timeout": timeout})
        return f"{self.base}/c/{urllib.parse.quote(consumer, safe='')}/mtproto/v1/getUpdates?{query}"

    def invoke(self, request, timeout=90):
        """Sends a Telethon request; returns its result. tgin's own short waits
        (a 503 with Retry-After, 429 too_many_in_flight or rate_limited; the
        call was not executed) are retried after Retry-After, up to 60 s,
        `attempts` times in all. Telegram's flood and slow-mode waits, media
        and init flood waits, and a 502 no_answer (it may have run) raise."""
        for attempt in range(1, self.attempts + 1):
            try:
                return self._invoke_once(request, timeout)
            except TginError as e:
                own = e.status == 503 or e.error in ("too_many_in_flight", "rate_limited")
                if attempt == self.attempts or not own or e.retry_after is None or e.retry_after > 60:
                    raise
                time.sleep(e.retry_after)

    def _invoke_once(self, request, timeout):
        headers = {"Authorization": f"Bearer {self.token}", "X-Tgin-Layer": str(self.layer),
                   "Content-Type": "application/octet-stream"}
        req = urllib.request.Request(self.invoke_url(), bytes(request), headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                status, body, head = resp.status, resp.read(), resp.headers
        except urllib.error.HTTPError as e:
            status, body, head = e.code, e.read(), e.headers
        retry = int(head["Retry-After"]) if (head.get("Retry-After") or "").isdigit() else None
        if head.get_content_type() == "application/json":  # tgin's own error
            err = json.loads(body)
            raise TginError(status, err.get("error"), err.get("description", ""), retry)
        if body[:4] == struct.pack("<I", RPC_ERROR):  # Telegram's rpc_error (200, 403 or 429)
            reader = BinaryReader(body[4:])
            code = reader.read_int()
            raise TginError(status, reader.tgread_string(), f"rpc_error {code}", retry)
        return request.read_result(BinaryReader(body))


def parse_envelope(envelope):
    """(update, users, chats) of one envelope, as Telethon objects."""
    return tuple(BinaryReader(base64.b64decode(envelope[k])).tgread_object() for k in ("tl", "users", "chats"))


def poll(tgin, consumer, consumer_token, instance, timeout=30):
    """Yields (envelope, update, users, chats) forever, acknowledging each poll.
    An envelope Telethon cannot parse (a constructor of another layer) is
    logged by its constructor and skipped, so polling moves past it."""
    offset = 0
    headers = {"Authorization": f"Bearer {consumer_token}", "X-Tgin-Instance": instance}
    while True:
        req = urllib.request.Request(tgin.updates_url(consumer, offset, timeout), headers=headers)
        with urllib.request.urlopen(req, timeout=timeout + 10) as resp:
            answer = json.loads(resp.read())
        for envelope in answer["result"]:
            offset = envelope["update_id"] + 1  # acknowledges everything up to this id; ids skip
            try:
                parsed = parse_envelope(envelope)
            except TypeNotFoundError as e:  # never log str(e): it holds the account's bytes
                log.warning("skipped update %s: constructor %08x is not in Telethon's layer",
                            envelope["update_id"], e.invalid_constructor_id)
                continue
            yield (envelope, *parsed)

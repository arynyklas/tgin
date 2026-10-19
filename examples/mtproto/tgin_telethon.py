"""A thin Telethon adapter for tgin's MTProto endpoints.

Calls go to `POST /mtproto/v1/{account}/invoke` as raw TL; updates come from
a pull consumer at `/c/{consumer}/mtproto/v1/getUpdates`, as envelope v1,
whose base64 TL Telethon's `BinaryReader` parses. Telethon serializes in its
own layer, so use it only for objects that layer shares with the account's
(the envelope's `layer`); see README.md.
"""

import base64
import json
import struct
import urllib.error
import urllib.parse
import urllib.request

from telethon.extensions import BinaryReader

RPC_ERROR = 0x2144CA19


class TginError(Exception):
    """tgin's own refusal ({"error","description"}) or Telegram's rpc_error."""

    def __init__(self, status, error, description="", retry_after=None):
        super().__init__(f"{status} {error}: {description}")
        self.status, self.error, self.retry_after = status, error, retry_after


class Tgin:
    def __init__(self, base_url, account, token, layer):
        self.base, self.account, self.token, self.layer = base_url.rstrip("/"), account, token, layer

    def invoke_url(self):
        return f"{self.base}/mtproto/v1/{urllib.parse.quote(self.account, safe='')}/invoke"

    def updates_url(self, consumer, offset, timeout):
        query = urllib.parse.urlencode({"offset": offset, "timeout": timeout})
        return f"{self.base}/c/{urllib.parse.quote(consumer, safe='')}/mtproto/v1/getUpdates?{query}"

    def invoke(self, request, timeout=90):
        """Sends a Telethon request object; returns its parsed result."""
        headers = {
            "Authorization": f"Bearer {self.token}",
            "X-Tgin-Layer": str(self.layer),
            "Content-Type": "application/octet-stream",
        }
        req = urllib.request.Request(self.invoke_url(), bytes(request), headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                status, body, ctype, retry = resp.status, resp.read(), resp.headers.get_content_type(), None
        except urllib.error.HTTPError as e:
            status, body, ctype, retry = e.code, e.read(), e.headers.get_content_type(), e.headers.get("Retry-After")
        if ctype == "application/json":  # tgin's own error: 400, 401, 403, 409, 429, 502, 503...
            err = json.loads(body)
            raise TginError(status, err.get("error"), err.get("description", ""), retry)
        if body[:4] == struct.pack("<I", RPC_ERROR):  # Telegram's rpc_error (200, 403 or 429)
            reader = BinaryReader(body[4:])
            code = reader.read_int()
            raise TginError(status, reader.tgread_string(), f"rpc_error {code}", retry)
        return request.read_result(BinaryReader(body))


def parse_envelope(envelope):
    """(update, users, chats) of one envelope, as Telethon objects."""
    tl = BinaryReader(base64.b64decode(envelope["tl"])).tgread_object()
    users = BinaryReader(base64.b64decode(envelope["users"])).tgread_object()
    chats = BinaryReader(base64.b64decode(envelope["chats"])).tgread_object()
    return tl, users, chats


def poll(tgin, consumer, consumer_token, instance, timeout=30):
    """Yields (envelope, update, users, chats) forever, acknowledging each poll."""
    offset = 0
    headers = {"Authorization": f"Bearer {consumer_token}", "X-Tgin-Instance": instance}
    while True:
        req = urllib.request.Request(tgin.updates_url(consumer, offset, timeout), headers=headers)
        with urllib.request.urlopen(req, timeout=timeout + 10) as resp:
            answer = json.loads(resp.read())
        for envelope in answer["result"]:
            offset = envelope["update_id"] + 1  # acknowledges everything up to this id; ids skip
            yield (envelope, *parse_envelope(envelope))

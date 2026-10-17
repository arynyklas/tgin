"""The webhook downstream of real_telegram.py: tgin's pushes land here.

It records what each push carried (the path, the `Via`, `X-Tgin-Source` and
`X-Tgin-Attempt` headers, whether the secret matched, the update's id and
text) and answers the first message on `/hook` that contains
"tgin-smoke-reply" with a webhook reply, a `sendMessage` to the same chat;
every other push gets an empty `200`. Records never hold a secret.

Standalone, for a look at what tgin sends:
    python hook.py --port 18081 --secret <the endpoint's secret_token>
"""

import argparse
import hmac
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPLY_MARKER = "tgin-smoke-reply"
REPLY_TEXT = "tgin smoke: this message is a webhook reply that tgin made"
SECRET_HEADER = "X-Telegram-Bot-Api-Secret-Token"


class Hook:
    """An HTTP server on 127.0.0.1 recording pushes; `secrets` maps each
    path to the secret token tgin must send on it."""

    def __init__(self, secrets, port=0):
        self.secrets = dict(secrets)
        self.records = []
        self._lock = threading.Lock()
        self._replied = False
        hook = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                hook._handle(self)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        self.port = self.server.server_address[1]
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self.server.shutdown()
        self.server.server_close()

    def url(self, path):
        return f"http://127.0.0.1:{self.port}{path}"

    def snapshot(self):
        with self._lock:
            return list(self.records)

    def _handle(self, req):
        length = int(req.headers.get("Content-Length") or 0)
        body = req.rfile.read(length)
        try:
            update = json.loads(body or b"{}")
        except ValueError:
            update = {}
        message = update.get("message") or {}
        text = message.get("text") or ""
        expected = self.secrets.get(req.path)
        sent = req.headers.get(SECRET_HEADER) or ""
        record = {
            "path": req.path,
            "via": req.headers.get_all("Via") or [],
            "source": req.headers.get("X-Tgin-Source"),
            "attempt": req.headers.get("X-Tgin-Attempt"),
            "secret_ok": expected is not None and hmac.compare_digest(sent, expected),
            "update_id": update.get("update_id"),
            "text": text,
        }
        reply = None
        with self._lock:
            self.records.append(record)
            if req.path == "/hook" and record["secret_ok"] and REPLY_MARKER in text and not self._replied:
                self._replied = True
                reply = json.dumps(
                    {"method": "sendMessage", "chat_id": message["chat"]["id"], "text": REPLY_TEXT}
                ).encode()
        req.send_response(200)
        if reply is None:
            req.send_header("Content-Length", "0")
            req.end_headers()
            return
        req.send_header("Content-Type", "application/json")
        req.send_header("Content-Length", str(len(reply)))
        req.end_headers()
        req.wfile.write(reply)


def main():
    parser = argparse.ArgumentParser(description="Record tgin's pushes and print them.")
    parser.add_argument("--port", type=int, default=18081)
    parser.add_argument("--secret", required=True, help="the secret_token tgin sends on /hook")
    args = parser.parse_args()
    hook = Hook({"/hook": args.secret}, port=args.port).start()
    print(f"listening on {hook.url('/hook')}; Ctrl+C stops", flush=True)
    seen = 0
    try:
        while True:
            threading.Event().wait(0.5)
            records = hook.snapshot()
            for record in records[seen:]:
                print(json.dumps(record), flush=True)
            seen = len(records)
    except KeyboardInterrupt:
        hook.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())

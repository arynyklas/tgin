#!/usr/bin/env python3
"""A manual check of tgin's Bot API gateway against the real Bot API, run
before a release with a test bot. Python 3.9+, standard library only. Never
run in CI.

It starts a built tgin with real_telegram.yaml: a long-poll bot `smoke` routed
to a pull consumer `workers`, a push consumer `hooks` that posts to hook.py
(which answers with a webhook reply), and a push consumer `captured` that
takes its endpoint from setWebhook. Then:

Phase A needs no chat. getMe from tgin's cache (also with the token's `:`
sent as `%3A`); getWebhookInfo, deleteWebhook and setWebhook answered by tgin
without reaching Telegram; getMyCommands, getMyName and getMyDescription
passed through unchanged; getUpdates refused on the bot path and served on
the pull consumer's path; unknown, double-encoded and out-of-scope tokens
refused with nothing forwarded; a setWebhook captured as a runtime endpoint;
the bot's profile photo downloaded through the file route; the metrics.

Phase B prints instructions for a person to send the bot a message that
contains "tgin-smoke-reply", then checks: the message arrives through the long
poll in the pull consumer; both push consumers got it with tgin's `Via`,
`X-Tgin-Source` and their secrets; hook.py's webhook reply reached Telegram;
four sends to the chat at once were shaped (burst 3, then 1 per second); a
200 KiB multipart sendDocument went up and came back byte for byte through
both file routes.

The bot must have no webhook and no pending updates (tgin long-polls it and
confirms what it takes); the script refuses to start otherwise. The token
comes from TGIN_SMOKE_TOKEN, or from the file given as the argument. It is
never printed: everything the script prints has it masked, and tgin's log is
checked for it at the end. tgin's config and log stay in a temporary work
directory, whose path the script prints; neither holds the token. `--api-url`
points the script at another Bot API, such as the fake in tests/e2e, for a
dry run. Exit code 0: every check passed; 1: a check failed; 2: the run
could not start or broke off.

    cargo build -p tgin
    TGIN_SMOKE_TOKEN=... python tests/smoke/real_telegram.py
    python tests/smoke/real_telegram.py ~/secrets/test-bot-token --phase a
"""

import argparse
import hashlib
import json
import os
import re
import secrets
import signal
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from hook import REPLY_MARKER, Hook

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
TOKEN_SHAPE = re.compile(r"^[0-9]+:[A-Za-z0-9_-]{1,128}$")
SEND_CONCURRENCY = 4


class Abort(Exception):
    """The run cannot go on; the message says why."""


class Masker:
    """Masks the token (and what is derived from it) in everything printed."""

    def __init__(self):
        self.words = []

    def add(self, *words):
        self.words.extend(w for w in words if w)
        self.words.sort(key=len, reverse=True)

    def __call__(self, text):
        text = str(text)
        for word in self.words:
            text = text.replace(word, "<redacted>")
        return text


MASK = Masker()


def say(*parts):
    print(MASK(" ".join(str(p) for p in parts)), flush=True)


class Checks:
    def __init__(self):
        self.failed = []
        self.passed = 0

    def __call__(self, name, ok, detail=""):
        detail = f"  ({MASK(detail)[:300]})" if detail != "" else ""
        say(("PASS " if ok else "FAIL ") + name + detail)
        if ok:
            self.passed += 1
        else:
            self.failed.append(name)
        return ok


CHECK = Checks()


def opener(url):
    """No proxy for loopback (tgin, a local fake), the environment's otherwise."""
    host = urllib.parse.urlsplit(url).hostname or ""
    if host in ("127.0.0.1", "localhost", "::1"):
        return urllib.request.build_opener(urllib.request.ProxyHandler({}))
    return urllib.request.build_opener()


def request(method, url, body=None, headers=None, timeout=40):
    """(status, headers, body bytes); HTTP errors are answers too."""
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with opener(url).open(req, timeout=timeout) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()


def call(base, method, params=None, timeout=40):
    """A Bot API call with a JSON body (or none): (status, parsed body)."""
    body = None if params is None else json.dumps(params).encode()
    headers = {} if params is None else {"Content-Type": "application/json"}
    status, _, raw = request("POST" if params is not None else "GET", f"{base}/{method}", body, headers, timeout)
    try:
        return status, json.loads(raw or b"null")
    except ValueError:
        return status, raw[:200]


def description(answer):
    return answer.get("description", "") if isinstance(answer, dict) else str(answer)


def multipart(fields, file_field, filename, content):
    boundary = "tgin-smoke-" + secrets.token_hex(12)
    parts = [
        f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode()
        for k, v in fields.items()
    ]
    parts.append(
        f'--{boundary}\r\nContent-Disposition: form-data; name="{file_field}"; filename="{filename}"\r\n'
        "Content-Type: application/octet-stream\r\n\r\n".encode() + content + b"\r\n"
    )
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def metrics(admin):
    """Every sample of tgin's /metrics, by its series."""
    _, _, raw = request("GET", f"{admin}/metrics", timeout=10)
    samples = {}
    for line in raw.decode().splitlines():
        if line and not line.startswith("#"):
            series, _, value = line.rpartition(" ")
            try:
                samples[series] = float(value)
            except ValueError:
                pass
    return samples


def metric(admin, name, **labels):
    """The sum of `name`'s samples whose labels include `labels`."""
    want = [f'{k}="{v}"' for k, v in labels.items()]
    total = 0.0
    for series, value in metrics(admin).items():
        family, _, rest = series.partition("{")
        if family == name and all(w in rest for w in want):
            total += value
    return total


def eventually(probe, within):
    deadline = time.monotonic() + within
    while True:
        result = probe()
        if result or time.monotonic() >= deadline:
            return result
        time.sleep(0.5)


class Run:
    def __init__(self, args, token):
        self.args = args
        self.token = token
        self.bot_id = int(token.split(":", 1)[0])
        self.api = args.api_url.rstrip("/")
        self.direct = f"{self.api}/bot{token}"
        self.data = f"http://127.0.0.1:{args.data_port}"
        self.admin = f"http://127.0.0.1:{args.admin_port}"
        self.gateway = f"{self.data}/bot{token}"
        self.hook_secret = secrets.token_urlsafe(24)
        self.capture_secret = secrets.token_urlsafe(24)
        MASK.add(self.hook_secret, self.capture_secret)
        self.hook = None
        self.tgin = None
        self.workdir = None
        self.log_path = None
        self.username = None
        self.offset = 0

    def scoped(self, consumer):
        return f"{self.data}/c/{consumer}/bot{self.token}"

    # --- set-up and tear-down -------------------------------------------------

    def precheck(self):
        status, me = call(self.direct, "getMe")
        if status != 200 or not isinstance(me, dict) or not me.get("ok"):
            raise Abort(f"the Bot API refused the token: {status} {description(me)}")
        self.username = me["result"].get("username")
        status, info = call(self.direct, "getWebhookInfo")
        if status != 200 or not info.get("ok"):
            raise Abort(f"getWebhookInfo failed: {status} {description(info)}")
        if info["result"].get("url"):
            raise Abort("the bot has a webhook; tgin's long poll would delete it. Use a test bot without one.")
        pending = info["result"].get("pending_update_count", 0)
        if pending and not self.args.allow_pending:
            raise Abort(
                f"the bot has {pending} pending updates, which tgin would take and confirm; "
                "use --allow-pending to accept that"
            )
        say(f"bot @{self.username} (id {self.bot_id}): no webhook, {pending} pending updates")

    def start(self):
        self.hook = Hook({"/hook": self.hook_secret, "/captured": self.capture_secret}).start()
        config = (HERE / "real_telegram.yaml").read_text(encoding="utf-8")
        for name, value in {
            "DATA": f"127.0.0.1:{self.args.data_port}",
            "ADMIN": f"127.0.0.1:{self.args.admin_port}",
            "API_URL": self.api,
            "HOOK_URL": self.hook.url("/hook"),
            "HOOK_SECRET": self.hook_secret,
        }.items():
            config = config.replace(f"@{name}@", value)
        self.workdir = Path(tempfile.mkdtemp(prefix="tgin-smoke-"))
        self.log_path = self.workdir / "tgin.log"
        config_path = self.workdir / "tgin.yaml"
        config_path.write_text(config, encoding="utf-8")
        env = {k: v for k, v in os.environ.items() if k != "RUST_LOG"}
        env["TGIN_SMOKE_TOKEN"] = self.token
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        log = open(self.log_path, "wb")
        self.tgin = subprocess.Popen(
            [str(self.args.tgin), "run", "-c", str(config_path)],
            stdout=log,
            stderr=subprocess.STDOUT,
            env=env,
            creationflags=flags,
        )
        log.close()
        ready = eventually(lambda: self.tgin.poll() is not None or self.ready(), 30)
        if self.tgin.poll() is not None or not ready:
            raise Abort("tgin did not become ready:\n" + self.log_tail())
        say(f"tgin ready: data {self.data}, admin {self.admin}, work directory {self.workdir}")

    def ready(self):
        try:
            return request("GET", f"{self.admin}/readyz", timeout=2)[0] == 200
        except OSError:
            return False

    def log_tail(self, lines=30):
        if self.log_path is None or not self.log_path.exists():
            return ""
        text = self.log_path.read_text(encoding="utf-8", errors="replace")
        return MASK("\n".join(text.splitlines()[-lines:]))

    def stop(self):
        if self.tgin is not None and self.tgin.poll() is None:
            try:
                self.tgin.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            except OSError:
                self.tgin.kill()
            try:
                self.tgin.wait(30)
            except subprocess.TimeoutExpired:
                self.tgin.kill()
                self.tgin.wait()
        if self.hook is not None:
            self.hook.stop()
            self.hook = None

    def scan_log(self):
        text = self.log_path.read_bytes().decode("utf-8", errors="replace")
        secret = self.token.split(":", 1)[1]
        hits = sum(text.count(w) for w in (self.token, secret, urllib.parse.quote(self.token, safe="")))
        leaked = sum(text.count(w) for w in (self.hook_secret, self.capture_secret))
        lines = text.splitlines()
        warnings = [line for line in lines if " WARN " in line or " ERROR " in line]
        CHECK(f"tgin's log ({len(lines)} lines) holds neither the token nor the bot's secret", hits == 0, f"{hits} hits")
        CHECK("tgin's log holds no push secret", leaked == 0, f"{leaked} hits")
        say(f"tgin's log has {len(warnings)} warning or error lines; it is {self.log_path}")
        for line in warnings[:10]:
            say("  " + line)

    # --- phase A --------------------------------------------------------------

    def phase_a(self):
        say("\nPHASE A (no chat needed)")
        status, me = call(self.gateway, "getMe")
        CHECK("getMe through the gateway is the bot", status == 200 and me["result"]["id"] == self.bot_id, status)
        local = eventually(
            lambda: call(self.gateway, "getMe")[0] == 200
            and metric(self.admin, "tgin_gateway_requests_total", method="getMe", outcome="local") >= 1,
            10,
        )
        CHECK("getMe is answered from tgin's cache (outcome local)", local)
        encoded = f"{self.data}/bot{urllib.parse.quote(self.token, safe='')}"
        status, me = call(encoded, "getMe")
        CHECK("a token with its ':' sent as %3A is accepted", status == 200 and me["result"]["id"] == self.bot_id, status)

        status, info = call(self.gateway, "getWebhookInfo")
        want = {"url": "", "has_custom_certificate": False, "pending_update_count": 0}
        CHECK("getWebhookInfo on the bot path is synthesized", status == 200 and info.get("result") == want, info)
        status, answer = call(self.gateway, "deleteWebhook", {})
        CHECK(
            "deleteWebhook answered by tgin (Telegram would say 'already deleted')",
            status == 200 and description(answer) == "Webhook was deleted",
            description(answer),
        )
        status, answer = call(self.gateway, "setWebhook", {"url": self.hook.url("/bot-path")})
        CHECK("setWebhook on the bot path answered by tgin", status == 200 and description(answer) == "Webhook was set", description(answer))
        status, info = call(self.direct, "getWebhookInfo")
        CHECK("Telegram still has no webhook", status == 200 and info["result"].get("url") == "", status)

        for method in ("getMyCommands", "getMyName", "getMyDescription"):
            via_tgin, direct = call(self.gateway, method), call(self.direct, method)
            CHECK(f"{method} through the gateway equals Telegram's answer", via_tgin == direct, via_tgin[0])

        status, answer = call(self.gateway, "getUpdates")
        CHECK(
            "getUpdates on the bot path is refused with 409",
            status == 409 and description(answer).startswith("Conflict: tgin serves getUpdates per consumer"),
            status,
        )
        status, answer = call(self.scoped("workers"), "getUpdates?timeout=0")
        CHECK("getUpdates on /c/workers/ is served from the pull queue", status == 200 and answer.get("ok") is True, status)
        status, answer = call(self.scoped("hooks"), "getUpdates?timeout=0")
        CHECK("getUpdates on a push consumer's path is 409", status == 409, description(answer))

        unknown = f"{self.bot_id}:{secrets.token_urlsafe(26)}"
        before = metric(self.admin, "tgin_gateway_unauthorized_total")
        status, _ = call(f"{self.data}/bot{unknown}", "getMe")
        CHECK("an unknown token gets 401", status == 401, status)
        secret = self.token.split(":", 1)[1]
        status, _ = call(f"{self.data}/bot{self.bot_id}%253A{secret}", "getMe")
        CHECK("a double-encoded token gets 404", status == 404, status)
        status, answer = call(self.scoped("nowhere"), "getMe")
        CHECK("a consumer the bot does not route to gets 404", status == 404, description(answer))
        after = metric(self.admin, "tgin_gateway_unauthorized_total")
        CHECK("tgin_gateway_unauthorized_total counted the unknown token once", after - before == 1, after - before)

        captured = self.scoped("captured")
        status, answer = call(captured, "setWebhook", {"url": "bad url!", "secret_token": self.capture_secret})
        CHECK("a bad webhook URL is refused like Telegram does", status == 400, description(answer))
        params = {"url": self.hook.url("/captured"), "secret_token": self.capture_secret}
        status, answer = call(captured, "setWebhook", params)
        CHECK("setWebhook on /c/captured/ sets the runtime endpoint", status == 200 and description(answer) == "Webhook was set", description(answer))
        status, answer = call(captured, "setWebhook", params)
        CHECK("the same setWebhook again is 'already set'", description(answer) == "Webhook is already set", description(answer))
        status, info = call(captured, "getWebhookInfo")
        CHECK("getWebhookInfo on /c/captured/ shows the runtime endpoint", info["result"]["url"] == self.hook.url("/captured"), info)
        status, raw = request("GET", f"{self.admin}/v1/status", timeout=10)[0::2]
        endpoints = next(
            (c.get("endpoints", []) for c in json.loads(raw).get("consumers", []) if c["name"] == "captured"), []
        )
        CHECK("/v1/status lists it as runtime", [e.get("runtime") for e in endpoints] == [True], endpoints)
        status, info = call(self.direct, "getWebhookInfo")
        CHECK("Telegram still has no webhook after the capture", info["result"].get("url") == "", status)

        status, photos = call(self.gateway, "getUserProfilePhotos", {"user_id": self.bot_id, "limit": 1})
        sizes = photos["result"]["photos"] if status == 200 and photos.get("ok") and photos["result"]["photos"] else None
        if sizes:
            smallest = min(sizes[0], key=lambda p: p.get("file_size", 0))
            self.download_check("the bot's profile photo", smallest["file_id"], None)
        else:
            say("SKIP profile photo download: the bot has no photo (phase B downloads a document)")

        samples = metrics(self.admin)
        counted = {
            s.partition("{")[2].rstrip("}"): v
            for s, v in samples.items()
            if s.startswith("tgin_gateway_requests_total{") and 'method="getMyCommands"' in s and v
        }
        CHECK("metrics: the proxied getMyCommands counted once", sum(counted.values()) == 1, counted)
        CHECK(
            "metrics: setWebhook answered locally",
            metric(self.admin, "tgin_gateway_requests_total", method="setWebhook", outcome="local") >= 3,
        )

    def download_check(self, what, file_id, expected):
        """Downloads `file_id` through both file routes and directly; all equal (and `expected`)."""
        status, info = call(self.gateway, "getFile", {"file_id": file_id})
        if not CHECK(f"getFile for {what}", status == 200 and info.get("ok"), status):
            return
        path = info["result"]["file_path"]
        quoted = urllib.parse.quote(path)
        bodies = {
            "/file/bot…": request("GET", f"{self.data}/file/bot{self.token}/{quoted}"),
            "/c/workers/file/bot…": request("GET", f"{self.data}/c/workers/file/bot{self.token}/{quoted}"),
            "Telegram": request("GET", f"{self.api}/file/bot{self.token}/{quoted}"),
        }
        digests = {k: (s, hashlib.sha256(b).hexdigest()) for k, (s, _, b) in bodies.items()}
        reference = expected if expected is not None else bodies["Telegram"][2]
        same = all(s == 200 and d == hashlib.sha256(reference).hexdigest() for s, d in digests.values())
        CHECK(f"{what} downloads byte for byte through both file routes", same, f"{len(reference)} bytes, {digests}")

    # --- phase B --------------------------------------------------------------

    def phase_b(self):
        say("\nPHASE B (needs a person)")
        say(
            f">>> From any Telegram account, send @{self.username} a message that contains\n"
            f">>>     {REPLY_MARKER}\n"
            f">>> (for example '{REPLY_MARKER} hello'). Waiting up to {self.args.wait} s."
        )
        found = None
        deadline = time.monotonic() + self.args.wait
        workers = self.scoped("workers")
        while found is None and time.monotonic() < deadline:
            status, answer = call(workers, f"getUpdates?timeout=10&offset={self.offset}", timeout=30)
            for update in answer.get("result", []) if status == 200 else []:
                self.offset = update["update_id"] + 1
                text = (update.get("message") or {}).get("text") or ""
                if found is None and REPLY_MARKER in text:
                    found = update
        if not CHECK("the message arrived through the long poll in the pull consumer", found is not None):
            return
        call(workers, f"getUpdates?timeout=0&offset={self.offset}")
        chat = found["message"]["chat"]["id"]
        update_id = found["update_id"]
        say(f"update {update_id} from chat {chat}")

        def pushed(path):
            return next((r for r in self.hook.snapshot() if r["path"] == path and r["update_id"] == update_id), None)

        for path, consumer in (("/hook", "hooks"), ("/captured", "captured")):
            record = eventually(lambda: pushed(path), 20)
            CHECK(
                f"{consumer} pushed the update with Via, X-Tgin-Source and its secret",
                record is not None
                and record["secret_ok"]
                and record["source"] == "smoke"
                and any(v.startswith("1.1 tgin-") for v in record["via"]),
                record,
            )
        executed = eventually(
            lambda: metric(self.admin, "tgin_webhook_replies_total", consumer="hooks", outcome="executed") >= 1, 20
        )
        CHECK("hook.py's webhook reply was made on Telegram (a sendMessage)", executed)

        time.sleep(3)  # the chat's quota is full again after the reply
        delayed_before = metric(self.admin, "tgin_gateway_ratelimited_total", scope="chat", action="delayed")
        results = {}

        def send(i):
            start = time.monotonic()
            text = f"tgin smoke: shaped send {i}/{SEND_CONCURRENCY} (burst 3, then 1 per second)"
            try:
                status, answer = call(self.gateway, "sendMessage", {"chat_id": chat, "text": text})
            except Exception as err:  # noqa: BLE001 - recorded masked, never raised in the thread
                status, answer = None, MASK(repr(err))
            results[i] = (time.monotonic() - start, status, answer)

        threads = [threading.Thread(target=send, args=(i,)) for i in range(1, SEND_CONCURRENCY + 1)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        durations = sorted(r[0] for r in results.values())
        CHECK(
            f"{SEND_CONCURRENCY} sends to the chat at once all succeed",
            len(results) == SEND_CONCURRENCY
            and all(r[1] == 200 and isinstance(r[2], dict) and r[2].get("ok") for r in results.values()),
            {i: r[1] if r[1] is not None else r[2] for i, r in sorted(results.items())},
        )
        CHECK(
            "the last send was shaped by about 1 s",
            len(durations) == SEND_CONCURRENCY and durations[-1] - durations[0] >= 0.8,
            [round(d, 2) for d in durations],
        )
        delayed = metric(self.admin, "tgin_gateway_ratelimited_total", scope="chat", action="delayed") - delayed_before
        CHECK("metrics: the chat quota's delay was counted", delayed >= 1, delayed)

        content = os.urandom(200 * 1024)
        body, content_type = multipart(
            {"chat_id": str(chat), "caption": "tgin smoke: a multipart upload through the gateway"},
            "document",
            "tgin-smoke.bin",
            content,
        )
        status, _, raw = request("POST", f"{workers}/sendDocument", body, {"Content-Type": content_type}, timeout=120)
        answer = json.loads(raw or b"null")
        if CHECK("sendDocument (multipart, 200 KiB) through /c/workers/", status == 200 and answer.get("ok"), description(answer)):
            self.download_check("the uploaded document", answer["result"]["document"]["file_id"], content)

        status, answer = call(self.scoped("captured"), "deleteWebhook", {})
        CHECK("deleteWebhook on /c/captured/ removes the runtime endpoint", description(answer) == "Webhook was deleted", description(answer))
        CHECK(
            "metrics: downloads counted under method file",
            metric(self.admin, "tgin_gateway_requests_total", method="file", outcome="ok") >= 2,
        )
        say(
            "In the chat the bot should now have sent, in order: the webhook reply, "
            f"{SEND_CONCURRENCY} 'shaped send' messages and the document."
        )


def read_token(args):
    if args.token_file is not None:
        try:
            token = Path(args.token_file).read_text(encoding="ascii").strip()
        except (OSError, UnicodeDecodeError) as err:
            raise Abort(f"cannot read the token file {args.token_file}: {type(err).__name__}") from None
    else:
        token = os.environ.get("TGIN_SMOKE_TOKEN", "").strip()
    if not token:
        raise Abort("no token: set TGIN_SMOKE_TOKEN or give a token file")
    if not TOKEN_SHAPE.match(token):
        raise Abort("the token is not shaped <digits>:<secret>")
    return token


def main():
    exe = "tgin.exe" if os.name == "nt" else "tgin"
    parser = argparse.ArgumentParser(
        description="Check tgin's Bot API gateway against the real Bot API (manual, before a release).",
        epilog="The token comes from TGIN_SMOKE_TOKEN unless a token file is given. It is never printed.",
    )
    parser.add_argument("token_file", nargs="?", help="a file holding the test bot's token")
    parser.add_argument("--tgin", type=Path, default=ROOT / "target" / "debug" / exe, help="the tgin binary (default: %(default)s)")
    parser.add_argument("--phase", choices=("a", "all"), default="all", help="'a': phase A only; 'all': A, then B (default)")
    parser.add_argument("--wait", type=int, default=300, help="seconds phase B waits for the message (default: %(default)s)")
    parser.add_argument("--api-url", default="https://api.telegram.org", help="the Bot API (default: %(default)s)")
    parser.add_argument("--data-port", type=int, default=18080, help="tgin's data plane on 127.0.0.1 (default: %(default)s)")
    parser.add_argument("--admin-port", type=int, default=19090, help="tgin's admin plane on 127.0.0.1 (default: %(default)s)")
    parser.add_argument("--allow-pending", action="store_true", help="run although the bot has pending updates (tgin takes them)")
    args = parser.parse_args()

    run = None
    try:
        token = read_token(args)
        MASK.add(token, token.split(":", 1)[1], urllib.parse.quote(token, safe=""))
        if not args.tgin.is_file():
            raise Abort(f"no tgin binary at {args.tgin}; build it (cargo build -p tgin) or pass --tgin")
        run = Run(args, token)
        run.precheck()
        run.start()
        try:
            run.phase_a()
            if args.phase == "all":
                run.phase_b()
        finally:
            run.stop()
        say("\nAFTER")
        run.scan_log()
        status, info = call(run.direct, "getWebhookInfo")
        CHECK("the bot is left without a webhook", status == 200 and info["result"].get("url") == "", status)
        if status == 200:
            say(f"pending updates left at Telegram: {info['result'].get('pending_update_count')}")
    except Abort as err:
        say(f"ABORT: {err}")
        if run is not None:
            run.stop()
        return 2
    except Exception:  # noqa: BLE001 - print it masked, never raw
        say("ERROR:\n" + traceback.format_exc())
        if run is not None:
            run.stop()
        return 2
    say(f"\nRESULT: {CHECK.passed} passed, {len(CHECK.failed)} failed" + (": " + ", ".join(CHECK.failed) if CHECK.failed else ""))
    return 1 if CHECK.failed else 0


if __name__ == "__main__":
    sys.exit(main())

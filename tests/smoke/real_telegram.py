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

Self-hosted (`--self-hosted URL --files-dir DIR`): the bot talks to the
operator's telegram-bot-api server, run with `--local`, through an upstream
`kind: local, local_mode: true` whose files tgin serves from DIR, the server's
`--files-dir` (its `--dir` when that is unset) as this machine sees it
(`--server-dir` names it as the server sees it, when the server runs in a
container). The server's api_id and api_hash stay with the server; the
script never reads them. Phases A and B run against
the server, and also check: getFile answers an absolute path from the server
and a relative one through tgin, and every download equals the file in the
server's directory; a `file:` reference (JSON, query string, multipart) is
refused by tgin; the bot's tgin_bot_info and tgin_upstream_up; a 53 MiB
document (over the cloud's 50 MB) goes up and comes back byte for byte; then
a reload switches the bot to webhook ingress, which the server delivers over
plain http to `--public-url`, and a person sends a second message, containing
"tgin-smoke-hook"; a last reload goes back to long poll, which deletes the
server's webhook.

`--migrate` (with `--self-hosted`; explicit, and asks for confirmation):
tgin starts with the bot on the cloud (`--api-url`), `tgin admin bot logout
smoke --yes` drains it, logs it out of the cloud and parks it, and a reload
moves it to the local server, where it stays. After the logout, the bot
cannot log in to the cloud Bot API again for 10 minutes.

`--fake`: the Bot API server is tgin's fake (tests/e2e/fake-telegram), for a
dry run: the script injects phase B's messages through the fake's control
API instead of waiting for a person, and checks in its call journal that no
`file:` reference reached it.

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

tgin must see the server's files directory under the server's own names,
which keep the token's ':'. A server on the same Linux or macOS host:

    python tests/smoke/real_telegram.py ~/secrets/test-bot-token \\
        --self-hosted http://127.0.0.1:8081 --files-dir /var/lib/telegram-bot-api/files

On Windows, Docker Desktop shows a ':' written in a bind mount as U+F03A,
which tgin does not look for: run the script, with a Linux tgin (`docker
build -t tgin .`, then `docker create` and `docker cp` its
/usr/local/bin/tgin to target/linux/tgin), in a container on the server's
network (here `tba`, the server's name `telegram-bot-api`) and files volume
(`tba-files`, the server's --files-dir /var/lib/telegram-bot-api/files):

    docker run --rm -it --network tba --network-alias smoke \\
        -v "$PWD:/src:ro" -v tba-files:/mnt/tba:ro -v ~/secrets:/secrets:ro \\
        python:3.13-slim python -u /src/tests/smoke/real_telegram.py \\
        /secrets/test-bot-token --tgin /src/target/linux/tgin --listen-host 0.0.0.0 \\
        --self-hosted http://telegram-bot-api:8081 --files-dir /mnt/tba \\
        --server-dir /var/lib/telegram-bot-api/files \\
        --public-url http://smoke:18080 [--migrate]
"""

import argparse
import hashlib
import json
import os
import re
import secrets
import signal
import socket
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
# The self-hosted phase's large document: over the cloud's 50 MB upload
# limit, under the fake's 64 MiB request body limit (a local-mode server
# takes up to 2000 MB).
BIG_DOCUMENT = 53 << 20
# Seconds a file transfer may take: an upload, a download, and getFile, which a
# local-mode server answers only once it has downloaded the whole file from
# Telegram.
TRANSFER_TIMEOUT = 600
HOOK_MARKER = "tgin-smoke-hook"
LOCAL_FILES_REFUSED = "Bad Request: local file uploads are disabled by tgin"
# --fake: the private chat the injected messages come from.
FAKE_CHAT = 424242001


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
    """(status, headers, body bytes); HTTP errors are answers too, and a
    timeout is the status "timeout" (a failed check, never a crash)."""
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with opener(url).open(req, timeout=timeout) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()
    except urllib.error.URLError as err:
        if isinstance(err.reason, (TimeoutError, socket.timeout)):
            return "timeout", {}, b""
        raise
    except (TimeoutError, socket.timeout):
        return "timeout", {}, b""


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


def result(answer):
    """A Bot API answer's `result` object, or {} (an error, a timeout, a non-object)."""
    value = answer.get("result") if isinstance(answer, dict) else None
    return value if isinstance(value, dict) else {}


def multipart(fields, file_field=None, filename=None, content=b""):
    """A multipart/form-data body: text `fields`, then one file part unless
    `file_field` is None."""
    boundary = "tgin-smoke-" + secrets.token_hex(12)
    parts = [
        f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode()
        for k, v in fields.items()
    ]
    if file_field is not None:
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
        self.local = args.self_hosted is not None
        self.cloud = args.api_url.rstrip("/")
        self.api = args.self_hosted.rstrip("/") if self.local else self.cloud
        self.direct = f"{self.api}/bot{token}"
        self.server = "the server" if self.local else "Telegram"
        if self.local:
            self.files_dir = Path(args.files_dir).resolve()
            # How the server names its directory in getFile's absolute paths.
            self.server_dir = (args.server_dir or str(self.files_dir)).rstrip("/\\") or "/"
            self.public_url = (args.public_url or f"http://127.0.0.1:{args.data_port}").rstrip("/")
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
        self.config_path = None
        self.username = None
        self.offset = 0
        self.message_id = 0

    def scoped(self, consumer):
        return f"{self.data}/c/{consumer}/bot{self.token}"

    # --- set-up and tear-down -------------------------------------------------

    def precheck(self, base, where):
        status, me = call(base, "getMe")
        if status != 200 or not isinstance(me, dict) or not me.get("ok"):
            raise Abort(f"{where} refused the token: {status} {description(me)}")
        self.username = me["result"].get("username")
        status, info = call(base, "getWebhookInfo")
        if status != 200 or not info.get("ok"):
            raise Abort(f"getWebhookInfo on {where} failed: {status} {description(info)}")
        if info["result"].get("url"):
            raise Abort(f"the bot has a webhook on {where}; tgin's long poll would delete it. Use a test bot without one.")
        pending = info["result"].get("pending_update_count", 0)
        if pending and not self.args.allow_pending:
            raise Abort(
                f"the bot has {pending} pending updates on {where}, which tgin would take and confirm; "
                "use --allow-pending to accept that"
            )
        say(f"bot @{self.username} (id {self.bot_id}) on {where}: no webhook, {pending} pending updates")

    def write_config(self, cloud=False, webhook=False):
        """tgin's config: the bot on the cloud (`--api-url`) unless it is on the
        self-hosted server; long poll unless `webhook`."""
        upstreams = ""
        if self.local:
            files = {"dir": str(self.files_dir)}
            if self.args.server_dir:
                files["server_dir"] = self.args.server_dir
            server = {"kind": "local", "url": self.api, "local_mode": True, "files": files}
            upstreams = "upstreams:\n  server: " + json.dumps(server)
        if self.local and not cloud:
            bot_api = "upstream: server"
        else:
            bot_api = "api_url: " + json.dumps(self.cloud)
        if webhook:
            ingress = "webhook: " + json.dumps({"public_url": self.public_url, "register": True})
        else:
            ingress = "long_poll: {timeout: 10s}"
        config = (HERE / "real_telegram.yaml").read_text(encoding="utf-8")
        for name, value in {
            "DATA": f"{self.args.listen_host}:{self.args.data_port}",
            "ADMIN": f"127.0.0.1:{self.args.admin_port}",
            "UPSTREAMS": upstreams,
            "BOT_API": bot_api,
            "INGRESS": ingress,
            "HOOK_URL": self.hook.url("/hook"),
            "HOOK_SECRET": self.hook_secret,
        }.items():
            config = config.replace(f"@{name}@", value)
        self.config_path.write_text(config, encoding="utf-8")

    def start(self):
        self.hook = Hook({"/hook": self.hook_secret, "/captured": self.capture_secret}).start()
        self.workdir = Path(tempfile.mkdtemp(prefix="tgin-smoke-"))
        self.log_path = self.workdir / "tgin.log"
        self.config_path = self.workdir / "tgin.yaml"
        self.write_config(cloud=self.args.migrate)
        env = {k: v for k, v in os.environ.items() if k != "RUST_LOG"}
        env["TGIN_SMOKE_TOKEN"] = self.token
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        log = open(self.log_path, "wb")
        self.tgin = subprocess.Popen(
            [str(self.args.tgin), "run", "-c", str(self.config_path)],
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

    def reload(self):
        """POST /v1/reload: (status, the reload report)."""
        status, _, raw = request("POST", f"{self.admin}/v1/reload", timeout=60)
        try:
            report = json.loads(raw or b"{}")
        except ValueError:
            report = {}
        return status, report if isinstance(report, dict) else {}

    def source(self):
        """The bot's entry in /v1/status (empty when tgin does not answer)."""
        try:
            status, _, raw = request("GET", f"{self.admin}/v1/status", timeout=10)
        except OSError:
            return {}
        sources = json.loads(raw).get("sources", []) if status == 200 else []
        return next((s for s in sources if s.get("name") == "smoke"), {})

    def server_webhook(self):
        """The server's getWebhookInfo result (empty when it fails)."""
        status, info = call(self.direct, "getWebhookInfo")
        return info["result"] if status == 200 and isinstance(info, dict) and info.get("ok") else {}

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
        forms = (self.token, secret, urllib.parse.quote(self.token, safe=""), self.token.replace(":", "~"))
        hits = sum(text.count(w) for w in forms)
        leaked = sum(text.count(w) for w in (self.hook_secret, self.capture_secret))
        lines = text.splitlines()
        warnings = [line for line in lines if " WARN " in line or " ERROR " in line]
        CHECK(f"tgin's log ({len(lines)} lines) holds neither the token nor the bot's secret", hits == 0, f"{hits} hits")
        CHECK("tgin's log holds no push secret", leaked == 0, f"{leaked} hits")
        say(f"tgin's log has {len(warnings)} warning or error lines; it is {self.log_path}")
        for line in warnings[:10]:
            say("  " + line)

    # --- migration --------------------------------------------------------------

    def confirm_migration(self):
        say(
            f"\n>>> --migrate logs @{self.username} out of the cloud Bot API ({self.cloud}) with\n"
            ">>>     tgin admin bot logout smoke --yes\n"
            ">>> and leaves it on the local server. For 10 minutes after the logout the bot\n"
            ">>> cannot log in to the cloud Bot API again."
        )
        try:
            answer = input(">>> Type 'logout' to go on: ")
        except EOFError:
            answer = ""
        if answer.strip() != "logout":
            raise Abort("migration not confirmed; nothing was changed")

    def probe_server(self):
        """An answer from the self-hosted server, asked without the token: any
        status below 500 (the server answers 404 to an unknown path). Aborts,
        before the logout, so nothing changes, on no answer or a 5xx, which a
        proxy in front of a dead server sends."""
        try:
            status = request("GET", f"{self.api}/", timeout=10)[0]
        except OSError as err:
            status = type(err).__name__
        if not isinstance(status, int) or status >= 500:
            raise Abort(f"the self-hosted server {self.api} does not answer ({status}); nothing was changed")
        say(f"the self-hosted server answers (HTTP {status} without a token)")

    def migrate(self):
        say("\nMIGRATION (--migrate)")
        env = {k: v for k, v in os.environ.items() if not k.startswith("TGIN_ADMIN_")}
        done = subprocess.run(
            [str(self.args.tgin), "admin", "--url", self.admin, "bot", "logout", "smoke", "--yes", "--timeout", "60s"],
            capture_output=True,
            text=True,
            env=env,
            timeout=180,
        )
        for line in (done.stdout + done.stderr).splitlines():
            say("  tgin admin: " + line)
        moved = CHECK(
            "`tgin admin bot logout smoke` logged the bot out of the cloud", done.returncode == 0, f"exit {done.returncode}"
        )
        source = self.source()
        CHECK("/v1/status shows the bot parked on the cloud", source.get("parked") is True, source.get("upstream"))
        if not moved:
            raise Abort("the bot is still on the cloud; nothing more was changed")
        say(
            "WARNING: the bot is logged out of the cloud Bot API. For the next 10 minutes it cannot log in\n"
            "there again; it stays on the local server."
        )
        self.precheck(self.direct, "the local server")
        self.write_config()
        status, report = self.reload()
        CHECK(
            "a reload moves the bot to the local server (upstream changed)",
            status == 200 and "smoke" in report.get("upstream_changed", []),
            status,
        )
        running = eventually(
            lambda: (lambda s: s.get("upstream") == "server" and not s.get("parked") and s.get("state") == "running")(
                self.source()
            ),
            30,
        )
        CHECK("the bot runs on the local server, no longer parked", running, self.source())

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
            f"deleteWebhook answered by tgin ({self.server} would say 'already deleted')",
            status == 200 and description(answer) == "Webhook was deleted",
            description(answer),
        )
        status, answer = call(self.gateway, "setWebhook", {"url": self.hook.url("/bot-path")})
        CHECK("setWebhook on the bot path answered by tgin", status == 200 and description(answer) == "Webhook was set", description(answer))
        status, info = call(self.direct, "getWebhookInfo")
        CHECK(f"{self.server} still has no webhook", status == 200 and info["result"].get("url") == "", status)

        for method in ("getMyCommands", "getMyName", "getMyDescription"):
            via_tgin, direct = call(self.gateway, method), call(self.direct, method)
            CHECK(f"{method} through the gateway equals {self.server}'s answer", via_tgin == direct, via_tgin[0])

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
        CHECK("getWebhookInfo on /c/captured/ shows the runtime endpoint", result(info).get("url") == self.hook.url("/captured"), info)
        status, raw = request("GET", f"{self.admin}/v1/status", timeout=10)[0::2]
        try:
            consumers = json.loads(raw).get("consumers", []) if status == 200 else []
        except ValueError:
            consumers = []
        endpoints = next((c.get("endpoints", []) for c in consumers if c["name"] == "captured"), [])
        CHECK("/v1/status lists it as runtime", [e.get("runtime") for e in endpoints] == [True], endpoints or status)
        status, info = call(self.direct, "getWebhookInfo")
        CHECK(f"{self.server} still has no webhook after the capture", status == 200 and result(info).get("url") == "", status)

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
        if self.local:
            self.self_hosted_a()

    def self_hosted_a(self):
        """Phase A's self-hosted checks: the bot's upstream, its metrics, `file:` refusals."""
        source = self.source()
        CHECK(
            "/v1/status: the bot long-polls the upstream `server`",
            source.get("upstream") == "server" and source.get("ingress") == "long_poll" and not source.get("parked"),
            source,
        )
        info = metric(self.admin, "tgin_bot_info", bot="smoke", upstream="server")
        CHECK('metrics: tgin_bot_info{bot="smoke",upstream="server"} is 1', info == 1, info)
        up = metric(self.admin, "tgin_upstream_up", bot="smoke")
        CHECK('metrics: tgin_upstream_up{bot="smoke"} is 1', up == 1, up)

        reference = "file:///nonexistent/tgin-smoke/refused.bin"
        before = metric(self.admin, "tgin_gateway_requests_total", method="sendDocument", outcome="rejected")
        status, answer = call(self.gateway, "sendDocument", {"chat_id": self.bot_id, "document": reference})
        CHECK(
            "a `file:` reference in a JSON body is refused by tgin",
            status == 400 and description(answer) == LOCAL_FILES_REFUSED,
            f"{status} {description(answer)}",
        )
        query = urllib.parse.urlencode({"chat_id": self.bot_id, "document": reference})
        status, answer = call(self.gateway, f"sendDocument?{query}")
        CHECK(
            "a `file:` reference in the query string is refused by tgin",
            status == 400 and description(answer) == LOCAL_FILES_REFUSED,
            f"{status} {description(answer)}",
        )
        body, content_type = multipart({"chat_id": str(self.bot_id), "document": reference})
        status, _, raw = request("POST", f"{self.gateway}/sendDocument", body, {"Content-Type": content_type})
        try:
            answer = json.loads(raw)
        except ValueError:
            answer = raw[:200]
        CHECK(
            "a `file:` reference in a multipart text field is refused by tgin",
            status == 400 and description(answer) == LOCAL_FILES_REFUSED,
            f"{status} {description(answer)}",
        )
        after = metric(self.admin, "tgin_gateway_requests_total", method="sendDocument", outcome="rejected")
        CHECK("metrics: the three refusals counted as rejected", after - before == 3, after - before)
        if self.args.fake:
            query = urllib.parse.urlencode({"token": self.token, "method": "sendDocument"})
            status, _, raw = request("GET", f"{self.api}/fake/calls?{query}", timeout=10)
            calls = json.loads(raw) if status == 200 else None
            CHECK(
                "the fake's journal: no `file:` reference reached the server",
                isinstance(calls, list) and not any(c.get("local_file") for c in calls),
                status if calls is None else f"{len(calls)} sendDocument calls",
            )

    def download_check(self, what, file_id, expected):
        """Downloads `file_id` through both file routes, and from Telegram (or,
        self-hosted, reads it from the server's directory); all equal (and
        `expected`)."""
        status, info = call(self.gateway, "getFile", {"file_id": file_id}, timeout=TRANSFER_TIMEOUT)
        if not CHECK(f"getFile for {what}", status == 200 and info.get("ok"), status):
            return
        path = info["result"]["file_path"]
        quoted = urllib.parse.quote(path)
        bodies = {
            "/file/bot…": request("GET", f"{self.data}/file/bot{self.token}/{quoted}", timeout=TRANSFER_TIMEOUT),
            "/c/workers/file/bot…": request(
                "GET", f"{self.data}/c/workers/file/bot{self.token}/{quoted}", timeout=TRANSFER_TIMEOUT
            ),
        }
        if self.local:
            # Never printed: the server's path holds the bot's directory, named after the token.
            status, direct = call(self.direct, "getFile", {"file_id": file_id}, timeout=TRANSFER_TIMEOUT)
            server_path = direct["result"].get("file_path", "") if status == 200 and direct.get("ok") else ""
            disk = self.on_disk(server_path)
            relative = not path.startswith("/") and server_path.replace("\\", "/").endswith("/" + path)
            if not CHECK(
                f"getFile for {what}: the server's absolute path comes back relative through tgin",
                disk is not None and relative,
                f"through tgin: {path}",
            ):
                return
            try:
                bodies["the server's directory"] = (200, None, disk.read_bytes())
            except OSError as err:
                bodies["the server's directory"] = (type(err).__name__, None, b"")
            source = "the server's directory"
        else:
            bodies["Telegram"] = request("GET", f"{self.api}/file/bot{self.token}/{quoted}", timeout=TRANSFER_TIMEOUT)
            source = "Telegram"
        digests = {k: (s, hashlib.sha256(b).hexdigest()) for k, (s, _, b) in bodies.items()}
        reference = expected if expected is not None else bodies[source][2]
        same = all(s == 200 and d == hashlib.sha256(reference).hexdigest() for s, d in digests.values())
        CHECK(f"{what} downloads byte for byte through both file routes", same, f"{len(reference)} bytes, {digests}")

    def on_disk(self, server_path):
        """The file at the server's absolute `server_path`, in --files-dir; None
        outside the server's directory."""
        root = self.server_dir
        if not server_path.startswith(root):
            return None
        rest = server_path[len(root):]
        if root not in ("/", "\\") and rest[:1] not in ("/", "\\"):
            return None
        parts = [p for p in re.split(r"[/\\]", rest) if p]
        if not parts or any(p in (".", "..") for p in parts):
            return None
        return self.files_dir.joinpath(*parts)

    # --- phase B --------------------------------------------------------------

    def inject(self, text):
        """--fake: queue a message from a person through the fake's control API."""
        self.message_id += 1
        person = {"id": FAKE_CHAT, "is_bot": False, "first_name": "Smoke"}
        update = {
            "message": {
                "message_id": self.message_id,
                "date": int(time.time()),
                "chat": {"id": FAKE_CHAT, "type": "private", "first_name": "Smoke"},
                "from": person,
                "text": text,
            }
        }
        body = json.dumps(update).encode()
        status, _, raw = request(
            "POST", f"{self.api}/fake/updates/{self.token}", body, {"Content-Type": "application/json"}, timeout=10
        )
        if status != 200:
            raise Abort(f"the fake refused the injected update: {status} {raw[:200]!r}")
        say(f">>> --fake: injected a message '{text}' through the fake's control API")

    def await_message(self, marker):
        """The first update on /c/workers/ whose message contains `marker`,
        after asking a person for it (or injecting it with --fake)."""
        if self.args.fake:
            self.inject(f"{marker} (a dry run)")
        else:
            say(
                f">>> From any Telegram account, send @{self.username} a message that contains\n"
                f">>>     {marker}\n"
                f">>> (for example '{marker} hello'). Waiting up to {self.args.wait} s."
            )
        found = None
        deadline = time.monotonic() + self.args.wait
        workers = self.scoped("workers")
        while found is None and time.monotonic() < deadline:
            status, answer = call(workers, f"getUpdates?timeout=10&offset={self.offset}", timeout=30)
            for update in answer.get("result", []) if status == 200 else []:
                self.offset = update["update_id"] + 1
                text = (update.get("message") or {}).get("text") or ""
                if found is None and marker in text:
                    found = update
        if found is not None:
            call(workers, f"getUpdates?timeout=0&offset={self.offset}")
        return found

    def phase_b(self):
        say("\nPHASE B (needs a person)")
        found = self.await_message(REPLY_MARKER)
        if not CHECK("the message arrived through the long poll in the pull consumer", found is not None):
            return
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
        CHECK(f"hook.py's webhook reply was made on {self.server} (a sendMessage)", executed)

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

        self.upload_check(chat, 200 * 1024, "200 KiB", "a multipart upload through the gateway")
        if self.local:
            self.upload_check(
                chat,
                BIG_DOCUMENT,
                f"{BIG_DOCUMENT >> 20} MiB",
                "a document over the cloud's 50 MB upload limit, through the local server",
            )
            # Before the capture goes: the webhook phase's message is pushed to it too.
            self.webhook_ingress()

        status, answer = call(self.scoped("captured"), "deleteWebhook", {})
        CHECK("deleteWebhook on /c/captured/ removes the runtime endpoint", description(answer) == "Webhook was deleted", description(answer))
        CHECK(
            "metrics: downloads counted under method file",
            metric(self.admin, "tgin_gateway_requests_total", method="file", outcome="ok") >= 2,
        )
        documents = "two documents" if self.local else "the document"
        say(
            "In the chat the bot should now have sent, in order: the webhook reply, "
            f"{SEND_CONCURRENCY} 'shaped send' messages and {documents}."
        )

    def upload_check(self, chat, size, label, caption):
        """A multipart sendDocument of `size` random bytes through /c/workers/,
        then its downloads."""
        content = os.urandom(size)
        body, content_type = multipart(
            {"chat_id": str(chat), "caption": f"tgin smoke: {caption}"}, "document", "tgin-smoke.bin", content
        )
        workers = self.scoped("workers")
        status, _, raw = request(
            "POST", f"{workers}/sendDocument", body, {"Content-Type": content_type}, timeout=TRANSFER_TIMEOUT
        )
        try:
            answer = json.loads(raw or b"null")
        except ValueError:
            answer = raw[:200]
        ok = status == 200 and isinstance(answer, dict) and answer.get("ok")
        if CHECK(f"sendDocument (multipart, {label}) through /c/workers/", ok, f"{status} {description(answer)}"):
            self.download_check(f"the {label} document", answer["result"]["document"]["file_id"], content)

    def webhook_ingress(self):
        say("\nPHASE B, webhook ingress (needs a person)")
        self.write_config(webhook=True)
        status, report = self.reload()
        CHECK(
            "a reload switches the bot to webhook ingress",
            status == 200 and "smoke" in report.get("sources_restarted", []),
            status,
        )
        url = f"{self.public_url}/webhook/smoke"
        registered = eventually(lambda: self.server_webhook().get("url") == url, 30)
        CHECK(f"tgin registered its webhook on the server over plain http ({url})", registered and url.startswith("http://"))
        source = eventually(lambda: (lambda s: s if s.get("ingress") == "webhook" and s.get("state") == "running" else None)(self.source()), 30)
        CHECK("/v1/status: the bot's source is a running webhook", source is not None, self.source())
        errors_before = metric(self.admin, "tgin_source_errors_total", source="smoke")
        # The long poll has stopped: whatever the source takes in from here came over the webhook.
        updates_before = self.source().get("updates_total", 0)
        found = self.await_message(HOOK_MARKER)
        updates = self.source().get("updates_total", 0) - updates_before
        CHECK(
            "the message came over the server's plain-http webhook into tgin and out through the pull consumer",
            found is not None and updates >= 1,
            f"{updates} updates taken in by the webhook source",
        )
        errors = metric(self.admin, "tgin_source_errors_total", source="smoke") - errors_before
        CHECK("tgin refused none of the server's webhook requests (secret, allowlist, body)", errors == 0, errors)
        settled = eventually(
            lambda: (lambda i: i.get("url") == url and i.get("pending_update_count") == 0 and not i.get("last_error_message"))(
                self.server_webhook()
            ),
            20,
        )
        CHECK("the server reports nothing pending and no delivery error", settled, self.server_webhook().get("last_error_message", ""))

        self.write_config()
        status, report = self.reload()
        CHECK(
            "a reload switches the bot back to long poll",
            status == 200 and "smoke" in report.get("sources_restarted", []),
            status,
        )
        deleted = eventually(lambda: self.server_webhook().get("url") == "", 30)
        CHECK("the long poll deleted tgin's webhook on the server", deleted)


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


def check_args(args):
    if args.self_hosted is None:
        given = [f for f, v in (("--files-dir", args.files_dir), ("--server-dir", args.server_dir),
                                ("--public-url", args.public_url), ("--migrate", args.migrate)) if v]
        if given:
            raise Abort(f"{', '.join(given)} need --self-hosted")
        return
    if args.fake and args.migrate and urllib.parse.urlsplit(args.api_url).hostname == "api.telegram.org":
        raise Abort("--fake --migrate needs --api-url: the cloud-mode fake to log the bot out of")
    if args.files_dir is None:
        raise Abort(
            "--self-hosted needs --files-dir: the server's --files-dir (its --dir when that is unset) as this machine sees it"
        )
    if not Path(args.files_dir).is_dir():
        raise Abort(f"--files-dir {args.files_dir} is not a directory")
    if args.server_dir is not None and not args.server_dir.startswith(("/", "\\")) and not re.match(r"^[A-Za-z]:[/\\]", args.server_dir):
        raise Abort("--server-dir must be an absolute path: the server's --files-dir (its --dir when that is unset) as it sees it")


def main():
    exe = "tgin.exe" if os.name == "nt" else "tgin"
    parser = argparse.ArgumentParser(
        description="Check tgin's Bot API gateway against the real Bot API (manual, before a release).",
        epilog="The token comes from TGIN_SMOKE_TOKEN unless a token file is given. It is never printed.",
    )
    parser.add_argument("token_file", nargs="?", help="a file holding the test bot's token")
    parser.add_argument("--tgin", type=Path, default=ROOT / "target" / "debug" / exe, help="the tgin binary (default: %(default)s)")
    parser.add_argument("--phase", choices=("a", "all"), default="all", help="'a': phase A only; 'all': A, then B (default)")
    parser.add_argument("--wait", type=int, default=300, help="seconds phase B waits for each message (default: %(default)s)")
    parser.add_argument("--api-url", default="https://api.telegram.org", help="the cloud Bot API (default: %(default)s)")
    parser.add_argument("--data-port", type=int, default=18080, help="tgin's data plane port (default: %(default)s)")
    parser.add_argument("--admin-port", type=int, default=19090, help="tgin's admin plane on 127.0.0.1 (default: %(default)s)")
    parser.add_argument(
        "--listen-host", default="127.0.0.1", help="the data plane's listen address (default: %(default)s)"
    )
    parser.add_argument("--allow-pending", action="store_true", help="run although the bot has pending updates (tgin takes them)")
    group = parser.add_argument_group("self-hosted server (telegram-bot-api --local)")
    group.add_argument("--self-hosted", metavar="URL", help="the server's base URL, as tgin and this script reach it")
    group.add_argument(
        "--files-dir",
        metavar="DIR",
        help="the server's --files-dir (its --dir when that is unset) as this machine sees it; tgin serves files from it",
    )
    group.add_argument(
        "--server-dir",
        metavar="PATH",
        help="the server's --files-dir (its --dir when that is unset) as the server sees it, when it differs (a container)",
    )
    group.add_argument(
        "--public-url",
        metavar="URL",
        help="the base URL the server posts webhooks to tgin at, plain http (default: http://127.0.0.1:<data-port>)",
    )
    group.add_argument(
        "--migrate",
        action="store_true",
        help="start on the cloud and move the bot with `tgin admin bot logout` (asks first; 10-minute cloud lockout)",
    )
    parser.add_argument(
        "--fake",
        action="store_true",
        help="the Bot API is tgin's fake (a dry run): inject phase B's messages through its control API",
    )
    args = parser.parse_args()

    run = None
    try:
        check_args(args)
        token = read_token(args)
        MASK.add(token, token.split(":", 1)[1], urllib.parse.quote(token, safe=""), token.replace(":", "~"))
        if not args.tgin.is_file():
            raise Abort(f"no tgin binary at {args.tgin}; build it (cargo build -p tgin) or pass --tgin")
        run = Run(args, token)
        if args.migrate:
            run.precheck(f"{run.cloud}/bot{token}", "the cloud Bot API")
            # Before the question: a run that would abort never asks for the logout.
            run.probe_server()
            run.confirm_migration()
        else:
            run.precheck(run.direct, "the local server" if run.local else "the Bot API")
        try:
            # Inside: an interrupt while tgin becomes ready still stops it.
            run.start()
            if args.migrate:
                run.migrate()
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
            say(f"pending updates left at {run.server}: {info['result'].get('pending_update_count')}")
        if args.migrate:
            say(
                "The bot stays on the local server, logged out of the cloud Bot API. To go back after the\n"
                "10 minutes: `tgin admin bot close smoke --yes` on a tgin running it on the server, then\n"
                "point the bot at the cloud again."
            )
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

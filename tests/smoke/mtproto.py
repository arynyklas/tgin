#!/usr/bin/env python3
"""A manual check of tgin's user accounts (MTProto) against Telegram, on the
owner's own account and a test bot. Never run in CI.

It drives a built tgin (`--tgin`, default target/release/tgin) whose config
has one account, `smoke`, on the existing authorization in `--session-dir`;
one pull consumer, `updates`, that every update of the account is routed to;
and one RPC client, `smoke`. The config goes to a temporary work directory
and holds no secret: every secret and the session directory are `${env:…}`
references, filled from the script's own environment for the child only.
The admin token, the RPC client's token and the consumer's token are made
fresh for each run and never printed or written to a file.

Run it with Telethon, from the adapter's uv project, which the script imports
(examples/mtproto/tgin_telethon.py):

    cargo build --release -p tgin
    uv run --project examples/mtproto python tests/smoke/mtproto.py
    uv run --project examples/mtproto python tests/smoke/mtproto.py --dry-run

Phases, in order:

Login (only when needed). The authorization is reused. The run refuses to
start when `<session-dir>/session.redb` is missing, so a wrong path never
leads into a new login, unless `--login` allows starting from an empty store.
When the store exists and `GET /v1/accounts` says the account is logged out,
the run stops, or with `--login` runs `tgin admin login smoke`: the owner
scans its QR in the account's app and types the 2FA password into tgin's own
prompt, which reads the console itself. The script never sees the password;
it hands tgin the admin token through a pipe, never a file, and relays the
login's stdout and stderr line by line through its masker (tgin prints the
logged-in user's id). An account that needs a new login (`needs_login`)
stops the run: that takes a logout, which the script never does. A session
held by another process is an error, never retried; any other failure tgin
retries by itself (`retrying`) is waited out, and one it does not retry stops
the run. Every wait for the account is bounded by `--catch-up-wait`: the
account is ready once it runs with its common box caught up, and channels
still catching up (a channel's flood wait) are a note, never a failure.

Updates. After the consumer has drained what the account caught up, the bot
sends silent messages to its private chat with the account. Each must arrive
on the pull consumer as an envelope of the account's layer whose `tl`,
`users` and `chats` Telethon decodes, whose constructor names the decoded
object, and whose message is marked silent. The bot then deletes them.

RPC. Through the adapter: a wrapped first call
(`invokeWithLayer(<layer>, initConnection(…, help.getNearestDc))`) is
answered with the inner call's result, so tgin stripped the wrappers it owns
(the `initConnection` carries a placeholder `api_id`, which Telegram would
refuse had it been forwarded); `messages.sendMessage` to Saved Messages
(`inputPeerSelf`), then `messages.deleteMessages` of what it sent. Refused
without reaching Telegram: `X-Tgin-Layer` naming another layer (409
`layer_mismatch`), an `invokeWithLayer` naming another layer (409), another
wrapper (400 `wrapper_refused`), `updates.getState`, `getDifference` and
`getChannelDifference` (403 `updates_method`, though the client's allow lists
them), and a wrong client token (401). `tgin_mtproto_rpc_total` must show the
refusals as `refused` and only the three real calls as `ok`.

Restart replay (common box: the bot's private chat). DOCS § Delivery: a
box's state is persisted only once its batch is admitted, refused batches
roll the box back and are fetched again, and queues live in memory, so a kill
loses envelopes that are queued and not yet handed to a consumer. The window
this phase targets is therefore: every update tgin admitted is leased to the
consumer and not acknowledged, and every later update is received and
refused (not admitted). tgin restarts with a consumer of capacity
`TIGHT_CAPACITY` and `on_full: backpressure`; the script leases from it under
fresh instance names, without acknowledging, until it holds the whole
capacity (`tgin_consumer_leased` at capacity, `tgin_consumer_queue_depth` 0),
sending filler messages if the account is quiet. The bot then sends more
messages, which the full queue refuses. tgin is killed hard (`Popen.kill()`:
SIGKILL, TerminateProcess on Windows) only once all of these hold:
`tgin_consumer_backpressure_seconds_total` grew after the last send (a
refusal after it, of the push or of the refetch that brings it); the box the
test messages go to is catching up in `/v1/accounts` (the common box for
private, a channel box for channel), since a refused batch rolls its box
back and fetches it again, while the queue's counter grows for any refused
batch; the queue still holds nothing unleased. Otherwise the window was not
reached, and the phase reports INCONCLUSIVE rather than pass or fail. tgin
then starts again on the same session. No message may be lost: each must
have been leased before the kill or arrive after the restart. The run
reports how many came back after the restart and how many it saw twice. A
missing message counts as lost only when the window was reached, Telethon
read every update of the phase, and the box is no longer catching up;
otherwise the result is INCONCLUSIVE. The one window DOCS says loses updates
(admitted, queued and not yet leased) is avoided by construction, and
checked before the kill.

Channel box. The same, with the bot's posts in the test channel and the
account's channel box (`getChannelDifference` at start for a channel the
last run left behind). DOCS still marks channel boxes "to be confirmed", so
the result is printed as a finding, whichever it is.

Throughout. Only the test bot sends test messages, all silent
(`disable_notification`), at most `--max-bot-messages` (hard cap 30) per run,
paced, and deleted once the account has seen them; the RPC writes only to
Saved Messages and deletes what it sent. A FLOOD_WAIT, a 429 or a
`retry_after` is waited out, never a failure. Only the private chat with the
bot, the test channel and Saved Messages have their content looked at, and
only the markers this run sent are printed; of every other update the run
keeps counts, constructor names, box kinds and pts, and never another chat's
id, name, text or media. Nothing printed holds the bot token or any part of
it, the api id or hash, a generated token, the account's user id or the
channel's id: chats are named `private`, `channel` and `saved`, and
everything printed is masked. tgin's logs are scanned for all of these at the
end. The authorization stays logged in: the script never logs out, and the
last tgin stops gracefully.

Secrets (defaults under ~/.tgin-secrets, `--secrets` moves them all):
`telegram-api.env` (`TELEGRAM_API_ID`, `TELEGRAM_API_HASH`), `bot-token`
(the test bot's token; `TGIN_SMOKE_TOKEN` overrides it) and `m3-gate.env`
(`TGIN_GATE_USER`: the account's user id, which is also the Bot API chat id
of its private chat with the bot; `TGIN_GATE_CHANNEL`: the test channel's
Bot API id, `-100…`). Variables in the environment override the files.

`--dry-run` reads no secret, opens no connection and touches no session: it
renders both configs into a temporary directory with placeholder values,
validates them with `tgin config validate` when the binary is there, checks
the Telethon decoding and the privacy filter on hand-built envelopes, the
account-state decisions on canned `/v1/accounts` entries, and the token pipe
with the masked relay of a child's output, and prints the phase plan. The
live run runs the same checks before it starts tgin.

Exit code 0: every check passed; 1: a check failed; 2: the run could not
start or broke off; 3: nothing failed, but a check was inconclusive.
"""

import argparse
import base64
import contextlib
import hashlib
import io
import json
import os
import re
import secrets
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
ADAPTER = ROOT / "examples" / "mtproto"
EXE = "tgin.exe" if os.name == "nt" else "tgin"

ACCOUNT = "smoke"
CONSUMER = "updates"
CLIENT = "smoke"
# The run's ceiling on test-bot messages; --max-bot-messages may only lower it.
BOT_MESSAGE_CAP = 30
# The replay phases' queue: small enough to fill with leases, so that later
# updates are refused instead of queued.
TIGHT_CAPACITY = 2
# Messages each phase sends.
UPDATES_MESSAGES = 3
REPLAY_AFTER_FULL = 3
# Seconds between two calls of the bot: under Telegram's per-chat and
# per-channel limits, so no flood wait is provoked.
BOT_PACE = 3.0
# The most envelopes one poll returns (getUpdates' `limit`).
PAGE = 100
TOKEN_SHAPE = re.compile(r"^[0-9]+:[A-Za-z0-9_-]{30,64}$")
API_HASH_SHAPE = re.compile(r"^[0-9a-f]{32}$")
# Constructors whose pts belongs to a channel's box (a pts beside a
# `channel_id`, or a message in a channel), for an update Telethon cannot
# decode; a decoded one is classified by its fields.
CHANNEL_BOX = frozenset(
    {
        "updateNewChannelMessage",
        "updateEditChannelMessage",
        "updateDeleteChannelMessages",
        "updateChannelWebPage",
        "updatePinnedChannelMessages",
        "updateChannelTooLong",
        "updateReadChannelInbox",
    }
)
VECTOR = 0x1CB5C415


class Abort(Exception):
    """The run cannot go on; the message says why."""


class Masker:
    """Masks every secret and identifier in everything printed."""

    def __init__(self):
        self.words = []

    def add(self, *words):
        self.words.extend(str(w) for w in words if w)
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
        self.inconclusive = []
        self.passed = 0

    def __call__(self, name, ok, detail=""):
        detail = f"  ({MASK(detail)[:300]})" if detail != "" else ""
        say(("PASS " if ok else "FAIL ") + name + detail)
        if ok:
            self.passed += 1
        else:
            self.failed.append(name)
        return ok

    def unsettled(self, name, why):
        """A check the run could not decide: neither passed nor failed."""
        say(f"INCONCLUSIVE {name}  ({MASK(why)[:300]})")
        self.inconclusive.append(name)


CHECK = Checks()


# --- HTTP ---------------------------------------------------------------------


def opener(url):
    """No proxy for loopback (tgin), the environment's otherwise (the Bot API)."""
    host = urllib.parse.urlsplit(url).hostname or ""
    if host in ("127.0.0.1", "localhost", "::1"):
        return urllib.request.build_opener(urllib.request.ProxyHandler({}))
    return urllib.request.build_opener()


def request(method, url, body=None, headers=None, timeout=40):
    """(status, headers, body bytes); HTTP errors are answers too, and a
    timeout is the status "timeout"."""
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


def json_of(raw):
    try:
        return json.loads(raw or b"null")
    except ValueError:
        return None


def eventually(probe, within, step=0.5):
    deadline = time.monotonic() + within
    while True:
        result = probe()
        if result or time.monotonic() >= deadline:
            return result
        time.sleep(step)


# --- secrets ------------------------------------------------------------------


def read_env_file(path):
    """KEY=VALUE lines; a missing file is empty. Values are never echoed."""
    values = {}
    try:
        text = Path(path).read_text(encoding="utf-8")
    except FileNotFoundError:
        return values
    except OSError as err:
        raise Abort(f"cannot read {path}: {type(err).__name__}") from None
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip().removeprefix("export ").strip()
        values[key] = value.strip().strip('"').strip("'")
    return values


def setting(name, file_values, where):
    value = os.environ.get(name, "").strip() or file_values.get(name, "")
    if not value:
        raise Abort(f"{name} is not set, in the environment or in {where}")
    return value


class Secrets:
    """Everything the live run must never print, and the ids it classifies by."""

    def __init__(self, args):
        api = read_env_file(args.api_env)
        gate = read_env_file(args.gate_env)
        self.api_id = setting("TELEGRAM_API_ID", api, args.api_env)
        self.api_hash = setting("TELEGRAM_API_HASH", api, args.api_env)
        if not self.api_id.isdigit() or int(self.api_id) < 1:
            raise Abort("TELEGRAM_API_ID is not a positive number")
        if not API_HASH_SHAPE.match(self.api_hash):
            raise Abort("TELEGRAM_API_HASH is not 32 hex digits")
        token = os.environ.get("TGIN_SMOKE_TOKEN", "").strip()
        if not token:
            try:
                token = Path(args.token_file).read_text(encoding="ascii").strip()
            except (OSError, UnicodeDecodeError) as err:
                raise Abort(f"cannot read the token file {args.token_file}: {type(err).__name__}") from None
        if not TOKEN_SHAPE.match(token):
            raise Abort("the bot token is not shaped <digits>:<secret>")
        self.token = token
        self.bot_id = int(token.split(":", 1)[0])
        user = setting("TGIN_GATE_USER", gate, args.gate_env)
        channel = setting("TGIN_GATE_CHANNEL", gate, args.gate_env)
        if not user.isdigit():
            raise Abort("TGIN_GATE_USER is not a user id")
        if not re.fullmatch(r"-100[0-9]+", channel):
            raise Abort("TGIN_GATE_CHANNEL is not a channel's Bot API id (-100…)")
        self.user_id = int(user)
        self.channel_chat = int(channel)
        self.channel_id = channel_id_of(self.channel_chat)
        # Made here, used here: tgin gets them through its environment.
        self.admin_token = secrets.token_urlsafe(32)
        self.rpc_token = secrets.token_urlsafe(32)
        self.pull_token = secrets.token_urlsafe(32)
        MASK.add(
            token,
            token.split(":", 1)[1],
            urllib.parse.quote(token, safe=""),
            self.bot_id,
            self.api_hash,
            self.api_id,
            self.user_id,
            self.channel_chat,
            self.channel_id,
            self.admin_token,
            self.rpc_token,
            self.pull_token,
        )

    def ids(self):
        return Ids(bot=self.bot_id, me=self.user_id, channel=self.channel_id)

    def leaks(self):
        """(what, pattern) of every value tgin's log must not hold. Numbers
        match only as whole numbers."""
        def number(n):
            return re.compile(rf"(?<![0-9]){n}(?![0-9])")

        def literal(s):
            return re.compile(re.escape(s))

        return [
            ("the bot token", literal(self.token.split(":", 1)[1])),
            ("the bot's id", number(self.bot_id)),
            ("the api hash", literal(self.api_hash)),
            ("the api id", number(self.api_id)),
            ("the account's user id", number(self.user_id)),
            ("the channel's id", number(self.channel_id)),
            ("the channel's Bot API id", number(self.channel_chat)),
            ("a generated token", re.compile("|".join(map(re.escape, (self.admin_token, self.rpc_token, self.pull_token))))),
        ]


def channel_id_of(chat_id):
    """A channel's MTProto id from its Bot API id (-100…)."""
    return -chat_id - 1_000_000_000_000


# --- envelopes ----------------------------------------------------------------


class Ids:
    """The three chats whose content the run may look at."""

    def __init__(self, bot, me, channel):
        self.bot, self.me, self.channel = bot, me, channel


def lower_first(name):
    return name[:1].lower() + name[1:]


def peer_of(obj, tl):
    """(kind, id) of the chat a decoded update belongs to, or None."""
    types = tl.types
    if isinstance(obj, types.UpdateShortMessage):
        return ("user", obj.user_id)
    if isinstance(obj, types.UpdateShortChatMessage):
        return ("chat", obj.chat_id)
    if isinstance(obj, types.UpdateShort):
        return peer_of(obj.update, tl)
    message = getattr(obj, "message", None)
    peer = getattr(message, "peer_id", None)
    if isinstance(peer, types.PeerUser):
        return ("user", peer.user_id)
    if isinstance(peer, types.PeerChat):
        return ("chat", peer.chat_id)
    if isinstance(peer, types.PeerChannel):
        return ("channel", peer.channel_id)
    channel = getattr(obj, "channel_id", None)
    if isinstance(channel, int):
        return ("channel", channel)
    return None


def chat_name(peer, ids):
    """`private`, `channel`, `saved`, or None for any other chat."""
    if peer == ("user", ids.bot):
        return "private"
    if peer == ("user", ids.me):
        return "saved"
    if peer == ("channel", ids.channel):
        return "channel"
    return None


def parse_blob(envelope, key, adapter):
    """One of the envelope's base64 TL fields as a Telethon object, or
    (None, why) when Telethon cannot read it. `why` is a constructor id or an
    error's class name, never the bytes."""
    try:
        return adapter.BinaryReader(base64.b64decode(envelope[key])).tgread_object(), None
    except adapter.TypeNotFoundError as err:
        return None, f"{key}:{err.invalid_constructor_id:08x}"
    except Exception as err:  # noqa: BLE001 - malformed bytes are a finding, not a crash
        return None, f"{key}:{type(err).__name__}"


def box_of(inner, tl):
    """The box a decoded update's sequence number belongs to, by its fields,
    as tgin files it: a pts beside a `channel_id`, or a pts of a message in a
    channel, is the channel's; any other pts or a qts is the common box's."""
    pts = getattr(inner, "pts", None)
    if pts is not None:
        message = getattr(inner, "message", None)
        in_channel = isinstance(getattr(message, "peer_id", None), tl.types.PeerChannel)
        return "channel" if isinstance(getattr(inner, "channel_id", None), int) or in_channel else "common"
    return "common" if getattr(inner, "qts", None) is not None else "none"


def classify(envelope, ids, tl, adapter):
    """What the run may keep of one envelope. Text and the silent flag only
    for the three test chats; of any other update, its constructor, box kind
    and pts. `decoded` only when `tl`, `users` and `chats` all parse; the
    chat and text come from `tl` alone, so a test-chat update whose entities
    Telethon cannot read is still recognised. Never raises."""
    constructor = envelope.get("constructor", "")
    seen = {
        "update_id": envelope.get("update_id"),
        "constructor": constructor,
        "chat": None,
        "box": "channel" if constructor in CHANNEL_BOX else "none",
        "pts": None,
        "decoded": False,
        "unknown": None,
        "names_match": False,
        "text": None,
        "silent": None,
        "account": envelope.get("account"),
        "layer": envelope.get("layer"),
    }
    update, why = parse_blob(envelope, "tl", adapter)
    if update is None:
        seen["unknown"] = why
        return seen
    inner = update.update if isinstance(update, tl.types.UpdateShort) else update
    seen["names_match"] = lower_first(type(inner).__name__) == constructor
    seen["pts"] = getattr(inner, "pts", None) or getattr(inner, "qts", None)
    seen["box"] = box_of(inner, tl)
    chat = chat_name(peer_of(update, tl), ids)
    seen["chat"] = chat
    if chat is not None:
        if isinstance(inner, tl.types.UpdateShortMessage):
            seen["text"], seen["silent"] = inner.message, inner.silent
        else:
            message = getattr(inner, "message", None)
            if isinstance(message, tl.types.Message):
                seen["text"], seen["silent"] = message.message, message.silent
    problems = [why for _, why in (parse_blob(envelope, k, adapter) for k in ("users", "chats")) if why]
    seen["unknown"] = ", ".join(problems) or None
    seen["decoded"] = not problems
    return seen


class Ledger:
    """The run's view of the consumer: the markers seen, and counts of all else."""

    def __init__(self, ids, tl, adapter):
        self.ids, self.tl, self.adapter = ids, tl, adapter
        self.markers = {}  # text -> [seen, …]
        self.others = Counter()  # (constructor, box) -> count
        self.undecoded = Counter()  # constructor id or error -> count
        self.other_pts = []

    def add(self, envelope, nonce):
        seen = classify(envelope, self.ids, self.tl, self.adapter)
        if not seen["decoded"]:
            self.undecoded[seen["unknown"]] += 1
        text = seen["text"]
        if seen["chat"] is not None and isinstance(text, str) and text.startswith(f"tgin-smoke {nonce} "):
            self.markers.setdefault(text, []).append(seen)
        else:
            # Nothing of another chat's update is kept beyond these.
            self.others[(seen["constructor"], seen["box"])] += 1
            if seen["pts"] is not None:
                self.other_pts.append((seen["box"], seen["pts"]))
        return seen

    def seen(self, text):
        return self.markers.get(text, [])

    def summary(self):
        total = sum(self.others.values())
        top = ", ".join(f"{c or '?'}/{b} x{n}" for (c, b), n in self.others.most_common(8))
        return f"{total} other updates" + (f" ({top})" if top else "")


# --- the token pipe -----------------------------------------------------------


def with_token_pipe(token, start_child, connect_timeout=30):
    """Starts `start_child(path)` with `path` naming a pipe that yields `token`
    once, to that child only, so the token is never in a file. Returns the
    child, still running."""
    if os.name == "nt":
        return _windows_token_pipe(token, start_child, connect_timeout)
    return _posix_token_pipe(token, start_child, connect_timeout)


def _windows_token_pipe(token, start_child, connect_timeout):
    import _winapi
    import ctypes
    from ctypes import wintypes

    pipe_access_outbound, pipe_reject_remote_clients = 0x2, 0x8
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetNamedPipeClientProcessId.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.ULONG)]
    kernel32.FlushFileBuffers.argtypes = [wintypes.HANDLE]
    name = r"\\.\pipe\tgin-smoke-" + secrets.token_hex(16)
    handle = _winapi.CreateNamedPipe(
        name,
        pipe_access_outbound | _winapi.FILE_FLAG_FIRST_PIPE_INSTANCE | _winapi.FILE_FLAG_OVERLAPPED,
        _winapi.PIPE_WAIT | pipe_reject_remote_clients,
        1,
        4096,
        4096,
        0,
        _winapi.NULL,
    )
    try:
        child = start_child(name)
        connected = _winapi.ConnectNamedPipe(handle, overlapped=True)
        deadline = time.monotonic() + connect_timeout
        while _winapi.WaitForSingleObject(connected.event, 200) != _winapi.WAIT_OBJECT_0:
            if child.poll() is not None or time.monotonic() > deadline:
                connected.cancel()
                connected.GetOverlappedResult(True)
                if child.poll() is None:
                    child.kill()
                    child.wait()
                raise Abort("the child never opened the token pipe")
        connected.GetOverlappedResult(True)
        pid = wintypes.ULONG()
        if not kernel32.GetNamedPipeClientProcessId(handle, ctypes.byref(pid)) or pid.value != child.pid:
            child.kill()
            child.wait()
            raise Abort("another process opened the token pipe; nothing was written to it")
        written, _ = _winapi.WriteFile(handle, token.encode(), overlapped=True)
        written.GetOverlappedResult(True)
        kernel32.FlushFileBuffers(handle)
        return child
    finally:
        _winapi.CloseHandle(handle)


def _posix_token_pipe(token, start_child, connect_timeout):
    import errno

    directory = tempfile.mkdtemp(prefix="tgin-smoke-pipe-")  # 0700
    path = os.path.join(directory, "token")
    os.mkfifo(path, 0o600)
    try:
        child = start_child(path)
        deadline = time.monotonic() + connect_timeout
        while True:
            try:
                fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
                break
            except OSError as err:
                if err.errno != errno.ENXIO:
                    raise
            if child.poll() is not None or time.monotonic() > deadline:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                raise Abort("the child never opened the token pipe")
            time.sleep(0.05)
        try:
            os.set_blocking(fd, True)
            os.write(fd, token.encode())
        finally:
            os.close(fd)
        return child
    finally:
        os.unlink(path)
        os.rmdir(directory)


def relay(stream, emit=say):
    """Each line of a child's output, through the masker, until EOF."""
    for line in stream:
        emit(line.rstrip("\n"))


def utf8_output(*streams):
    """Makes text streams write UTF-8 whatever the console is: tgin draws the
    login QR in Unicode half blocks, which a Windows console's code page (or
    cp1252 on a pipe or redirect) cannot encode."""
    for stream in streams:
        stream.reconfigure(encoding="utf-8", errors="replace")


def run_relayed(argv, token, env=None):
    """Runs `argv`, whose None names the token pipe, with stdin inherited and
    its stdout and stderr relayed line by line through the masker (`tgin admin
    login` prints the logged-in user's id; its errors quote tgin's and
    Telegram's messages). The QR is drawn on stdout and survives the relay;
    the 2FA prompt reads and writes the console itself, never these pipes.
    The exit code."""

    def start(path):
        return subprocess.Popen(
            [path if a is None else a for a in argv],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
        )

    child = with_token_pipe(token, start)
    try:
        relay(child.stdout)
    except BaseException:
        # The relay broke off (an error, Ctrl+C): never leave the child behind.
        if child.poll() is None:
            child.kill()
        child.wait()
        raise
    finally:
        child.stdout.close()
    return child.wait()


# --- the config ---------------------------------------------------------------

CONFIG = """\
version: 1
log: {{level: info}}
reload: {{watch: false}}
admin:
  listen: ["127.0.0.1:{admin_port}"]
  token: ${{env:TGIN_SMOKE_ADMIN_TOKEN}}
server:
  listen: ["127.0.0.1:{data_port}"]
accounts:
  {account}:
    api_id: ${{env:TGIN_SMOKE_API_ID}}
    api_hash: ${{env:TGIN_SMOKE_API_HASH}}
    session_dir: ${{env:TGIN_SMOKE_SESSION_DIR}}
    rpc:
      max_rps: 5
      flood_sleep_threshold: 10s
      clients:
        {client}:
          token: ${{env:TGIN_SMOKE_RPC_TOKEN}}
          # The update methods are listed to show they stay refused.
          allow:
            - messages.sendMessage
            - messages.deleteMessages
            - help.getNearestDc
            - updates.getState
            - updates.getDifference
            - updates.getChannelDifference
    routes:
      - to: [{consumer}]
consumers:
  {consumer}:
    pull:
      auth_token: ${{env:TGIN_SMOKE_PULL_TOKEN}}
      capacity: {capacity}
      capacity_bytes: 256MiB
      on_full: backpressure
      lease_timeout: 10m
"""


def render_config(data_port, admin_port, capacity):
    return CONFIG.format(
        admin_port=admin_port,
        data_port=data_port,
        account=ACCOUNT,
        client=CLIENT,
        consumer=CONSUMER,
        capacity=capacity,
    )


def write_configs(workdir, args):
    """tgin.yaml (the normal queue) and tight.yaml (the replay phases' queue)."""
    paths = {}
    for name, capacity in (("normal", 10000), ("tight", TIGHT_CAPACITY)):
        path = workdir / f"{name}.yaml"
        path.write_text(render_config(args.data_port, args.admin_port, capacity), encoding="utf-8")
        paths[name] = path
    return paths


# --- the plan -----------------------------------------------------------------


def plan(args):
    """(phase, bot messages at most) in order."""
    replay = TIGHT_CAPACITY + 1 + REPLAY_AFTER_FULL  # fillers, then the refused ones
    return [
        ("login: only when /v1/accounts says logged out and --login is given", 0),
        (f"updates: {UPDATES_MESSAGES} silent messages in private, decoded with Telethon, deleted", UPDATES_MESSAGES),
        ("rpc: wrapped first call, sendMessage to saved and its deletion, the refusals", 0),
        ("channel warm-up: one post in channel", 1),
        (f"restart replay, common box: up to {TIGHT_CAPACITY + 1} fillers, {REPLAY_AFTER_FULL} refused, kill, restart", replay),
        (f"channel box: up to {TIGHT_CAPACITY + 1} fillers, {REPLAY_AFTER_FULL} refused, kill, restart", replay),
    ]


def print_plan(args):
    total = 0
    say("PLAN")
    for i, (phase, messages) in enumerate(plan(args), 1):
        total += messages
        say(f"  {i}. {phase}" + (f"  [bot messages <= {messages}]" if messages else ""))
    say(f"  bot messages at most {total}, budget {args.max_bot_messages} (cap {BOT_MESSAGE_CAP})")
    if total > args.max_bot_messages:
        raise Abort(f"the plan needs up to {total} bot messages; --max-bot-messages is {args.max_bot_messages}")
    return total


# --- Telethon -----------------------------------------------------------------


def load_telethon():
    """(telethon.tl, the adapter module), or an Abort that says how to run."""
    sys.path.insert(0, str(ADAPTER))
    try:
        import tgin_telethon as adapter
        from telethon import tl
        from telethon.errors import TypeNotFoundError
    except ImportError as err:
        raise Abort(
            f"cannot import Telethon or the adapter ({err.name}); run with "
            "`uv run --project examples/mtproto python tests/smoke/mtproto.py`"
        ) from None
    adapter.TypeNotFoundError = TypeNotFoundError
    # The adapter calls urlopen: keep its loopback calls (and the RPC token)
    # off any proxy.
    urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))
    return tl, adapter


def self_check(tl, adapter):
    """Telethon's decoding and the privacy filter on hand-built envelopes, with
    made-up ids. Offline."""
    t = tl.types
    ids = Ids(bot=1001, me=1002, channel=1003)
    date = datetime(2026, 1, 1, tzinfo=timezone.utc)
    nonce = "selfcheck"
    empty = struct.pack("<Ii", VECTOR, 0)

    def envelope(obj, constructor):
        return {
            "update_id": 1,
            "account": ACCOUNT,
            "layer": 230,
            "constructor": constructor,
            "received_at": "2026-01-01T00:00:00.000Z",
            "tl": base64.b64encode(bytes(obj)).decode(),
            "users": base64.b64encode(empty).decode(),
            "chats": base64.b64encode(empty).decode(),
        }

    def message(peer, text, silent=True, out=False):
        return t.Message(id=7, peer_id=peer, date=date, message=text, out=out, silent=silent)

    cases = [
        # A catch-up update as tgin composes it: pts of the difference, pts_count 0.
        (envelope(t.UpdateNewMessage(message(t.PeerUser(1001), f"tgin-smoke {nonce} a"), 10, 0), "updateNewMessage"),
         {"chat": "private", "box": "common", "pts": 10, "silent": True, "text": f"tgin-smoke {nonce} a"}),
        (envelope(t.UpdateShortMessage(id=8, user_id=1001, message=f"tgin-smoke {nonce} b", pts=11, pts_count=1,
                                       date=date, silent=True), "updateShortMessage"),
         {"chat": "private", "box": "common", "pts": 11, "silent": True, "text": f"tgin-smoke {nonce} b"}),
        (envelope(t.UpdateNewChannelMessage(message(t.PeerChannel(1003), f"tgin-smoke {nonce} c"), 5, 1),
                  "updateNewChannelMessage"),
         {"chat": "channel", "box": "channel", "pts": 5, "text": f"tgin-smoke {nonce} c"}),
        (envelope(t.UpdateNewMessage(message(t.PeerUser(1002), f"tgin-smoke {nonce} d", out=True), 12, 1),
                  "updateNewMessage"),
         {"chat": "saved", "box": "common", "text": f"tgin-smoke {nonce} d"}),
        # Another chat: nothing but its constructor, box and pts is kept.
        (envelope(t.UpdateNewMessage(message(t.PeerUser(1004), f"tgin-smoke {nonce} e"), 13, 1), "updateNewMessage"),
         {"chat": None, "box": "common", "text": None, "silent": None}),
        (envelope(t.UpdateShort(update=t.UpdateUserStatus(1004, t.UserStatusRecently()), date=date), "updateUserStatus"),
         {"chat": None, "box": "none", "text": None, "names_match": True}),
        # A pts beside a channel_id: the channel's box.
        (envelope(t.UpdateReadChannelInbox(channel_id=1003, max_id=7, still_unread_count=0, pts=6),
                  "updateReadChannelInbox"),
         {"chat": "channel", "box": "channel", "pts": 6, "text": None}),
    ]
    ok = True
    for i, (env, want) in enumerate(cases):
        got = classify(env, ids, tl, adapter)
        bad = {k: got[k] for k, v in want.items() if got[k] != v}
        if not got["decoded"] or not got["names_match"] or bad:
            ok = False
            say(f"  self-check case {i}: decoded={got['decoded']} names_match={got['names_match']} differs={sorted(bad)}")
    junk = base64.b64encode(struct.pack("<I", 0xDEADBEEF)).decode()
    unknown = dict(cases[0][0], update_id=2, tl=junk)
    got = classify(unknown, ids, tl, adapter)
    ok &= not got["decoded"] and got["unknown"] == "tl:deadbeef" and got["text"] is None and got["chat"] is None
    # Entities Telethon cannot read: the marker is still recognised, but not decoded.
    bad_users = dict(cases[0][0], update_id=3, users=junk)
    got = classify(bad_users, ids, tl, adapter)
    ok &= (not got["decoded"] and got["unknown"] == "users:deadbeef" and got["chat"] == "private"
           and got["text"] == f"tgin-smoke {nonce} a")
    ledger = Ledger(ids, tl, adapter)
    for env, _ in cases:
        ledger.add(env, nonce)
    ledger.add(bad_users, nonce)
    ok &= sorted(ledger.markers) == [f"tgin-smoke {nonce} {c}" for c in "abcd"]
    ok &= [s["decoded"] for s in ledger.seen(f"tgin-smoke {nonce} a")] == [True, False]
    ok &= sum(ledger.undecoded.values()) == 1
    ok &= "tgin-smoke" not in ledger.summary() and sum(ledger.others.values()) == 3
    return CHECK("Telethon decodes hand-built envelopes; only the test chats' text is kept", bool(ok))


# --- account states -----------------------------------------------------------


def account_step(entry):
    """What the run does about the account's `/v1/accounts` entry, as
    (verdict, detail):
    - ("ready", None): running with the common box caught up (channels may
      still catch up);
    - ("logged_out", None): no login recorded and no operation of this
      process holds the session;
    - ("flood_wait", until or None): wait it out;
    - ("wait", None): anything that settles by itself (no answer yet,
      connecting, a common catch-up, a session held by this process's login
      or logout, a failure tgin retries);
    - ("abort", why): it needs the owner, or tgin does not retry it."""
    if not entry:
        return "wait", None
    state = entry.get("state")
    if state == "running":
        return ("wait", None) if (entry.get("catching_up") or {}).get("common", True) else ("ready", None)
    if state == "logged_out":
        return ("logged_out", None) if entry.get("session_held_by") is None else ("wait", None)
    if state == "flood_wait":
        return "flood_wait", entry.get("until")
    if state == "failed":
        reason = entry.get("reason")
        if reason == "session_held":
            return "abort", "the session is held by another process (the soak?); one process at a time, so this is not retried"
        if entry.get("retrying") is False:
            return "abort", f"the account failed for good: {reason}"
        return "wait", None
    if state == "needs_login":
        return "abort", (
            "Telegram no longer accepts the authorization (needs_login). A new login takes a logout first, "
            "which this script never does: the owner decides, then reruns with --login"
        )
    if state == "frozen":
        return "abort", "Telegram froze the account (frozen); the run stops"
    return "wait", None


def refetching(entry, chat):
    """Whether the box the chat's updates go to is catching up. /v1/accounts
    does not name channels, so for `channel` any channel box counts."""
    catching = (entry or {}).get("catching_up") or {}
    if chat == "private":
        return bool(catching.get("common"))
    return catching.get("channels", 0) >= 1


def state_checks():
    """The decisions on canned /v1/accounts entries: account_step, and
    wait_running and drain driven by them with the consumer stubbed, so
    nothing sleeps. Offline."""

    def entry(state, common=False, channels=0, **extra):
        return dict(name=ACCOUNT, state=state, layer=230, home_dc=2,
                    catching_up={"common": common, "channels": channels}, **extra)

    steps = [
        (None, "wait"),
        (entry("connecting"), "wait"),
        (entry("running", common=True), "wait"),
        (entry("running", channels=2), "ready"),
        (entry("logged_out"), "logged_out"),
        (entry("logged_out", session_held_by="login"), "wait"),
        (entry("flood_wait", until=77), "flood_wait"),
        (entry("failed", reason="connect", retrying=True), "wait"),
        (entry("failed", reason="panicked", retrying=True), "wait"),
        (entry("failed", reason="storage", retrying=True), "wait"),
        (entry("failed", reason="session_held", retrying=True), "abort"),
        (entry("failed", reason="init_refused", retrying=False), "abort"),
        (entry("failed", reason="duplicate_account", retrying=False), "abort"),
        (entry("needs_login"), "abort"),
        (entry("frozen"), "abort"),
    ]
    ok = all(account_step(e)[0] == want for e, want in steps)
    ok &= account_step(entry("flood_wait", until=77)) == ("flood_wait", 77)
    ok &= refetching(entry("running", common=True), "private") and not refetching(entry("running", channels=1), "private")
    ok &= refetching(entry("running", channels=1), "channel") and not refetching(entry("running", common=True), "channel")

    def stub(entries, catch_up_wait=60):
        """A run whose /v1/accounts answers `entries` in turn; asked once
        more, it ends the call with an Abort, so a wrong decision shows as a
        wrong outcome instead of a wait."""
        feed = iter(entries)
        taken = []

        def account():
            try:
                return next(feed)
            except StopIteration:
                raise Abort("asked past the canned states") from None

        s = SimpleNamespace(tgin=None, args=SimpleNamespace(catch_up_wait=catch_up_wait), log_tail=lambda: "",
                            idle=lambda: None)
        s.account = account
        s.take = lambda timeout=5: taken.append(timeout) or []
        return s, taken

    def outcome(call):
        try:
            return call()
        except Abort as err:
            return str(err)

    with contextlib.redirect_stdout(io.StringIO()):
        # A common catch-up takes from the consumer, then a channel still
        # catching up is ready, with a note.
        s, taken = stub([entry("running", common=True), entry("running", channels=1)])
        got = outcome(lambda: Run.wait_running(s))
        ok &= isinstance(got, dict) and got["catching_up"]["channels"] == 1 and taken == [1]
        # Failures tgin retries are waited out.
        s, _ = stub([entry("failed", reason="connect", retrying=True), entry("failed", reason="panicked", retrying=True),
                     entry("connecting"), entry("running")])
        ok &= isinstance(outcome(lambda: Run.wait_running(s)), dict)
        s, _ = stub([entry("failed", reason="session_held", retrying=True)])
        ok &= "held by another process" in outcome(lambda: Run.wait_running(s))
        s, _ = stub([entry("failed", reason="init_refused", retrying=False)])
        ok &= "failed for good: init_refused" in outcome(lambda: Run.wait_running(s))
        s, _ = stub([entry("needs_login")])
        ok &= "needs_login" in outcome(lambda: Run.wait_running(s))
        s, _ = stub([entry("logged_out")])
        ok &= outcome(lambda: Run.wait_running(s, allow_logged_out=True))["state"] == "logged_out"
        s, _ = stub([entry("running", common=True)], catch_up_wait=0)
        ok &= "did not run within" in outcome(lambda: Run.wait_running(s))
        # drain: caught up returns; only channels left at the deadline is a
        # note; the common box still catching up at the deadline aborts.
        s, _ = stub([entry("running")])
        ok &= outcome(lambda: Run.drain(s, 0)) == 0
        s, _ = stub([entry("running", channels=1)])
        ok &= outcome(lambda: Run.drain(s, 0)) == 0
        s, _ = stub([entry("running", common=True)])
        ok &= "did not drain" in outcome(lambda: Run.drain(s, 0))
        s, _ = stub([entry("failed", reason="init_refused", retrying=False)])
        ok &= "failed for good" in outcome(lambda: Run.drain(s, 0))
    return CHECK("account-state decisions on canned /v1/accounts entries", bool(ok))


# --- the run ------------------------------------------------------------------


class Run:
    def __init__(self, args, keys, tl, adapter):
        self.args, self.keys, self.tl, self.adapter = args, keys, tl, adapter
        self.ids = keys.ids()
        self.data = f"http://127.0.0.1:{args.data_port}"
        self.admin = f"http://127.0.0.1:{args.admin_port}"
        self.bot = f"{args.api_url.rstrip('/')}/bot{keys.token}"
        self.nonce = secrets.token_hex(4)
        self.workdir = Path(tempfile.mkdtemp(prefix="tgin-mtproto-smoke-"))
        self.configs = write_configs(self.workdir, args)
        self.tgin = None
        self.starts = 0
        self.logs = []
        self.layer = None
        self.ledger = Ledger(self.ids, tl, adapter)
        self.sent = []  # (chat name, message id) the bot sent and has not deleted
        self.bot_messages = 0
        self.last_bot_call = 0.0
        self.offset = 0
        self.instance_seq = 0
        self.saved_sent = []  # message ids the RPC sent to saved and has not deleted

    # --- tgin -----------------------------------------------------------------

    def env(self):
        env = {k: v for k, v in os.environ.items() if k != "RUST_LOG" and not k.startswith("TGIN_SMOKE_")}
        env.update(
            TGIN_SMOKE_ADMIN_TOKEN=self.keys.admin_token,
            TGIN_SMOKE_RPC_TOKEN=self.keys.rpc_token,
            TGIN_SMOKE_PULL_TOKEN=self.keys.pull_token,
            TGIN_SMOKE_API_ID=self.keys.api_id,
            TGIN_SMOKE_API_HASH=self.keys.api_hash,
            TGIN_SMOKE_SESSION_DIR=str(self.args.session_dir),
        )
        return env

    def start(self, config):
        self.starts += 1
        log_path = self.workdir / f"tgin-{self.starts}.log"
        self.logs.append(log_path)
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        with open(log_path, "wb") as log:
            self.tgin = subprocess.Popen(
                [str(self.args.tgin), "run", "-c", str(self.configs[config])],
                stdout=log,
                stderr=subprocess.STDOUT,
                env=self.env(),
                creationflags=flags,
            )
        up = eventually(lambda: self.tgin.poll() is not None or self.account() is not None, 30)
        if self.tgin.poll() is not None or not up:
            raise Abort("tgin did not start:\n" + self.log_tail())
        say(f"tgin #{self.starts} up ({config} queue), work directory {self.workdir}")

    def idle(self):
        """The pause between two looks at an account that is not running."""
        time.sleep(1)

    def stop(self):
        """Graceful: tgin commits what it admitted and releases the session."""
        if self.tgin is not None and self.tgin.poll() is None:
            try:
                self.tgin.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            except OSError:
                self.tgin.kill()
            try:
                self.tgin.wait(60)
            except subprocess.TimeoutExpired:
                self.tgin.kill()
                self.tgin.wait()
        self.tgin = None

    def kill(self):
        """Hard: no shutdown, no drain, no last write."""
        self.tgin.kill()
        self.tgin.wait(30)
        self.tgin = None

    def log_tail(self, lines=20):
        if not self.logs or not self.logs[-1].exists():
            return ""
        text = self.logs[-1].read_text(encoding="utf-8", errors="replace")
        return MASK("\n".join(text.splitlines()[-lines:]))

    def admin_get(self, path, timeout=10):
        try:
            status, _, raw = request(
                "GET", f"{self.admin}{path}", headers={"Authorization": f"Bearer {self.keys.admin_token}"}, timeout=timeout
            )
        except OSError:
            return None, None
        return status, raw

    def account(self):
        """The account's entry in /v1/accounts, or None while tgin does not answer."""
        status, raw = self.admin_get("/v1/accounts")
        if status != 200:
            return None
        answer = json_of(raw) or {}
        return next((a for a in answer.get("accounts", []) if a.get("name") == ACCOUNT), None)

    def metric(self, name, **labels):
        status, raw = self.admin_get("/metrics")
        if status != 200:
            return None
        want = [f'{k}="{v}"' for k, v in labels.items()]
        total = 0.0
        for line in raw.decode().splitlines():
            if not line or line.startswith("#"):
                continue
            series, _, value = line.rpartition(" ")
            family, _, rest = series.partition("{")
            if family == name and all(w in rest for w in want):
                try:
                    total += float(value)
                except ValueError:
                    pass
        return total

    def wait_running(self, within=None, allow_logged_out=False):
        """The account's entry once it runs with its common box caught up
        (channels may still catch up: a channel's flood wait is waited out by
        the phases that need it), or, with `allow_logged_out`, once it shows
        logged out. Takes from the consumer meanwhile, since a catch-up waits
        for room. Bounded by --catch-up-wait, extended past a flood wait."""
        within = self.args.catch_up_wait if within is None else within
        deadline = time.monotonic() + within
        last = None
        while True:
            if self.tgin is not None and self.tgin.poll() is not None:
                raise Abort("tgin exited:\n" + self.log_tail())
            entry = self.account()
            verdict, detail = account_step(entry)
            state = (entry or {}).get("state")
            if (state, verdict) != last:
                reason = (entry or {}).get("reason")
                say(f"account state: {state}" + (f" ({reason})" if reason else ""))
                last = (state, verdict)
            if verdict == "abort":
                raise Abort(detail)
            if verdict == "ready":
                channels = entry["catching_up"]["channels"]
                if channels:
                    say(f"note: {channels} channel box(es) still catching up; going on")
                return entry
            if verdict == "logged_out" and allow_logged_out:
                return entry
            if verdict == "flood_wait" and detail:
                # Waited out, never a failure.
                deadline = max(deadline, detail - time.time() + time.monotonic() + 60)
            if time.monotonic() >= deadline:
                raise Abort(f"the account did not run within {within}s (state {state})")
            if state == "running":
                self.take(timeout=1)
            else:
                self.idle()

    # --- the consumer ---------------------------------------------------------

    def poll(self, instance, offset, timeout, limit=PAGE):
        query = urllib.parse.urlencode({"offset": offset, "timeout": timeout, "limit": limit})
        status, _, raw = request(
            "GET",
            f"{self.data}/c/{CONSUMER}/mtproto/v1/getUpdates?{query}",
            headers={"Authorization": f"Bearer {self.keys.pull_token}", "X-Tgin-Instance": instance},
            timeout=timeout + 15,
        )
        answer = json_of(raw) if status == 200 else None
        if not isinstance(answer, dict) or not answer.get("ok"):
            raise Abort(f"getUpdates on the consumer answered {status}")
        return answer["result"]

    def take(self, timeout=5):
        """One poll that acknowledges the previous one: the envelopes, kept in the ledger."""
        envelopes = self.poll("smoke-main", self.offset, timeout)
        for envelope in envelopes:
            self.offset = max(self.offset, envelope["update_id"] + 1)
            self.ledger.add(envelope, self.nonce)
        return envelopes

    def lease(self):
        """One poll under a fresh instance, never acknowledged: its leases stay
        until the lease timeout or the process ends."""
        self.instance_seq += 1
        envelopes = self.poll(f"smoke-hold-{self.instance_seq}", 0, 1)
        for envelope in envelopes:
            self.ledger.add(envelope, self.nonce)
        return envelopes

    def drain(self, within):
        """Takes and acknowledges until the account has caught up and a poll
        no longer brings a full page (the owner's other chats never go quiet,
        so an empty poll is not required). Channels still catching up at the
        deadline are a note, not an abort: a channel's flood wait is waited
        out, never a failure. A common-box flood wait extends the deadline."""
        deadline = time.monotonic() + within
        taken = 0
        while True:
            got = self.take(timeout=2)
            taken += len(got)
            entry = self.account()
            verdict, detail = account_step(entry)
            if verdict == "abort":
                raise Abort(detail)
            if verdict == "flood_wait" and detail:
                deadline = max(deadline, detail - time.time() + time.monotonic() + 60)
            channels = entry["catching_up"]["channels"] if verdict == "ready" else None
            if channels == 0 and len(got) < PAGE:
                self.take(timeout=0)  # acknowledges the last batch
                return taken
            if time.monotonic() >= deadline:
                if channels:
                    say(f"note: {channels} channel box(es) still catching up after {within}s; going on")
                    self.take(timeout=0)
                    return taken
                raise Abort(f"the consumer did not drain within {within}s ({taken} updates taken)")

    def wait_for(self, texts, within):
        """Takes until every text in `texts` was seen; the ones still missing."""
        deadline = time.monotonic() + within
        while True:
            missing = [t for t in texts if not self.ledger.seen(t)]
            if not missing or time.monotonic() >= deadline:
                return missing
            self.take(timeout=min(10, max(1, int(deadline - time.monotonic()))))

    def wait_replay(self, texts, chat):
        """Takes until every text was seen. Past --wait it goes on only while
        the chat's box still catches up (a flood wait is waited out), up to
        --catch-up-wait. (missing, whether the box still catches up)."""
        start = time.monotonic()
        while True:
            missing = [t for t in texts if not self.ledger.seen(t)]
            if not missing:
                return missing, False
            elapsed = time.monotonic() - start
            if elapsed >= self.args.wait:
                catching = refetching(self.account(), chat)
                if not catching or elapsed >= self.args.catch_up_wait:
                    return missing, catching
            self.take(timeout=5)

    # --- the bot --------------------------------------------------------------

    def bot_call(self, method, params):
        """A paced Bot API call; a 429 is waited out and sent again."""
        while True:
            pause = self.last_bot_call + BOT_PACE - time.monotonic()
            if pause > 0:
                time.sleep(pause)
            self.last_bot_call = time.monotonic()
            status, _, raw = request(
                "POST", f"{self.bot}/{method}", json.dumps(params).encode(), {"Content-Type": "application/json"}
            )
            answer = json_of(raw)
            if isinstance(answer, dict) and answer.get("ok"):
                return answer["result"]
            retry = (answer or {}).get("parameters", {}).get("retry_after") if isinstance(answer, dict) else None
            if status == 429 or retry:
                wait = int(retry or 5)
                say(f"the Bot API asks to wait {wait}s ({method}); waiting")
                time.sleep(wait + 1)
                continue
            description = answer.get("description", "") if isinstance(answer, dict) else ""
            raise Abort(f"the bot's {method} failed: {status} {description}")

    def chat_id(self, chat):
        return self.keys.user_id if chat == "private" else self.keys.channel_chat

    def send(self, chat, tag):
        """A silent message from the bot; its marker text."""
        if self.bot_messages >= self.args.max_bot_messages:
            raise Abort(f"the run reached its {self.args.max_bot_messages} bot messages")
        text = f"tgin-smoke {self.nonce} {tag}"
        self.bot_messages += 1
        result = self.bot_call("sendMessage", {"chat_id": self.chat_id(chat), "text": text, "disable_notification": True})
        self.sent.append((chat, result["message_id"]))
        return text

    def delete_sent(self):
        """Deletes what the bot sent and has not deleted, one call per chat."""
        for chat in ("private", "channel"):
            ids = [m for c, m in self.sent if c == chat]
            for i in range(0, len(ids), 100):
                self.bot_call("deleteMessages", {"chat_id": self.chat_id(chat), "message_ids": ids[i:i + 100]})
            if ids:
                say(f"the bot deleted its {len(ids)} messages in {chat}")
        self.sent = []

    # --- RPC ------------------------------------------------------------------

    def rpc(self, layer=None, token=None):
        return self.adapter.Tgin(self.data, ACCOUNT, token or self.keys.rpc_token, layer or self.layer)

    def invoke(self, client, request):
        """Through the adapter; Telegram's flood waits are waited out."""
        while True:
            try:
                return client.invoke(request)
            except self.adapter.TginError as err:
                if err.status == 429 and err.retry_after is not None:
                    say(f"RPC asked to wait {err.retry_after}s ({err.error}); waiting")
                    time.sleep(err.retry_after + 1)
                    continue
                raise

    def refused(self, name, client, request, status, error):
        try:
            client.invoke(request)
        except self.adapter.TginError as err:
            return CHECK(name, (err.status, err.error) == (status, error), f"{err.status} {err.error}")
        return CHECK(name, False, "the call ran")

    # --- phases ---------------------------------------------------------------

    def phase_login(self):
        say("\nLOGIN")
        entry = self.wait_running(allow_logged_out=True)
        if entry["state"] != "logged_out":
            say("the existing authorization is in use; no login")
            return entry
        if not self.args.login:
            raise Abort(
                "the account is logged out. A login makes a new authorization: rerun with --login, "
                "and the owner scans the QR and types the 2FA password in tgin's prompt"
            )
        say(">>> The owner logs the account in now: scan the QR below in the account's app")
        say(">>> (Settings > Devices > Link Desktop Device), then type the 2FA password if tgin asks.")

        argv = [str(self.args.tgin), "admin", "--url", self.admin, "--token-file", None, "login", ACCOUNT]
        env = {k: v for k, v in os.environ.items() if not k.startswith(("TGIN_SMOKE_", "TGIN_ADMIN_"))}
        code = run_relayed(argv, self.keys.admin_token, env)
        CHECK("tgin admin login ended", code == 0, f"exit {code}")
        if code != 0:
            raise Abort("the login did not end logged in")
        return self.wait_running()

    def phase_updates(self):
        say("\nUPDATES")
        texts = [self.send("private", f"u{i}") for i in range(1, UPDATES_MESSAGES + 1)]
        missing = self.wait_for(texts, self.args.wait)
        CHECK(f"{len(texts)} messages from the bot arrived on the pull consumer", not missing, f"{len(missing)} missing")
        seen = [s for t in texts for s in self.ledger.seen(t)]
        CHECK("every one decodes with Telethon (tl, users, chats)", bool(seen) and all(s["decoded"] for s in seen))
        CHECK("the envelope's constructor names the decoded update", bool(seen) and all(s["names_match"] for s in seen),
              ", ".join(sorted({s["constructor"] for s in seen})))
        CHECK(f"account and layer {self.layer} on every envelope",
              bool(seen) and all(s["account"] == ACCOUNT and s["layer"] == self.layer for s in seen))
        CHECK("each is in private and marked silent", bool(seen) and all(s["chat"] == "private" and s["silent"] for s in seen))
        dups = sum(max(0, len(self.ledger.seen(t)) - 1) for t in texts)
        say(f"duplicates: {dups}; boxes: {sorted({s['box'] for s in seen})}")
        self.delete_sent()

    def phase_rpc(self):
        say("\nRPC")
        f, t = self.tl.functions, self.tl.types
        client = self.rpc()
        ok0 = self.metric("tgin_mtproto_rpc_total", account=ACCOUNT, client=CLIENT, outcome="ok") or 0
        refused0 = self.metric("tgin_mtproto_rpc_total", account=ACCOUNT, client=CLIENT, outcome="refused") or 0

        def init(layer, query):
            # A placeholder api_id: tgin strips initConnection, so Telegram never sees it.
            return f.InvokeWithLayerRequest(layer, f.InitConnectionRequest(
                api_id=1, device_model="tgin-smoke", system_version="smoke", app_version="1",
                system_lang_code="en", lang_pack="", lang_code="en", query=query))

        try:
            answer = self.invoke(client, init(self.layer, f.help.GetNearestDcRequest()))
            CHECK("a wrapped first call is answered with the inner call's result", isinstance(answer, t.NearestDc),
                  type(answer).__name__)
        except self.adapter.TginError as err:
            CHECK("a wrapped first call is answered with the inner call's result", False, f"{err.status} {err.error}")

        text = f"tgin-smoke {self.nonce} saved"
        sent = self.invoke(client, f.messages.SendMessageRequest(peer=t.InputPeerSelf(), message=text, silent=True))
        message_id = None
        if isinstance(sent, t.UpdateShortSentMessage):
            message_id = sent.id
        else:
            for update in getattr(sent, "updates", []):
                if isinstance(update, t.UpdateMessageID):
                    message_id = update.id
                elif isinstance(update, (t.UpdateNewMessage,)) and getattr(update.message, "message", None) == text:
                    message_id = update.message.id
        CHECK("messages.sendMessage to saved through the adapter", message_id is not None, type(sent).__name__)
        if message_id is not None:
            self.saved_sent.append(message_id)
            affected = self.invoke(client, f.messages.DeleteMessagesRequest(id=[message_id], revoke=True))
            CHECK("messages.deleteMessages removed it from saved", isinstance(affected, t.messages.AffectedMessages)
                  and affected.pts_count >= 1, type(affected).__name__)
            if isinstance(affected, t.messages.AffectedMessages):
                self.saved_sent.remove(message_id)

        other = self.rpc(layer=self.layer - 1)
        refusals = [
            ("X-Tgin-Layer of another layer: 409", other, f.help.GetNearestDcRequest(), 409, "layer_mismatch"),
            ("invokeWithLayer of another layer: 409", client, init(self.layer - 1, f.help.GetNearestDcRequest()), 409,
             "layer_mismatch"),
            ("another wrapper (invokeWithoutUpdates): 400", client, f.InvokeWithoutUpdatesRequest(f.help.GetNearestDcRequest()),
             400, "wrapper_refused"),
            ("updates.getState: 403 although allowed", client, f.updates.GetStateRequest(), 403, "updates_method"),
            ("updates.getDifference: 403 although allowed", client,
             f.updates.GetDifferenceRequest(pts=1, date=datetime(2026, 1, 1, tzinfo=timezone.utc), qts=0), 403,
             "updates_method"),
            ("updates.getChannelDifference: 403 although allowed", client,
             f.updates.GetChannelDifferenceRequest(channel=t.InputChannelEmpty(), filter=t.ChannelMessagesFilterEmpty(),
                                                   pts=1, limit=1), 403, "updates_method"),
        ]
        for name, who, req, status, error in refusals:
            self.refused(name, who, req, status, error)
        self.refused("a wrong client token: 401", self.rpc(token=secrets.token_urlsafe(32)), f.help.GetNearestDcRequest(),
                     401, "unauthorized")
        ok1 = self.metric("tgin_mtproto_rpc_total", account=ACCOUNT, client=CLIENT, outcome="ok") or 0
        refused1 = self.metric("tgin_mtproto_rpc_total", account=ACCOUNT, client=CLIENT, outcome="refused") or 0
        CHECK("the refusals never reached Telegram (rpc outcome refused)", refused1 - refused0 == len(refusals),
              f"refused +{refused1 - refused0:g}")
        CHECK("only the three real calls ran (rpc outcome ok)", ok1 - ok0 == 3, f"ok +{ok1 - ok0:g}")

    def phase_channel_warmup(self):
        say("\nCHANNEL WARM-UP")
        text = self.send("channel", "c0")
        missing = self.wait_for([text], self.args.wait)
        seen = self.ledger.seen(text)
        CHECK("a post in channel arrives on the consumer, in a channel box",
              not missing and all(s["box"] == "channel" and s["decoded"] for s in seen))
        self.delete_sent()

    def fill_and_kill(self, chat, prefix):
        """Fills the tight queue with leases, makes the account receive updates
        it must refuse, then kills tgin. (texts sent, how often each was seen
        before the kill, whether the window was reached, the undecoded count
        when the phase began)."""
        consumer = {"consumer": CONSUMER}
        undecoded0 = sum(self.ledger.undecoded.values())
        texts = []
        fillers = 0
        full = False
        while True:
            self.lease()
            leased = self.metric("tgin_consumer_leased", **consumer)
            depth = self.metric("tgin_consumer_queue_depth", **consumer)
            if leased is not None and leased >= TIGHT_CAPACITY and depth == 0:
                full = True
                break
            if fillers > TIGHT_CAPACITY:
                break
            fillers += 1
            texts.append(self.send(chat, f"{prefix}f{fillers}"))
            eventually(lambda: self.lease() or (self.metric("tgin_consumer_leased", **consumer) or 0) >= TIGHT_CAPACITY, 15, 1)
        say(f"{chat}: queue full of leased, unacknowledged updates: {full} (after {fillers} fillers)")
        pressure0 = self.metric("tgin_consumer_backpressure_seconds_total", **consumer) or 0
        for i in range(1, REPLAY_AFTER_FULL + 1):
            texts.append(self.send(chat, f"{prefix}r{i}"))
        after_sends = self.metric("tgin_consumer_backpressure_seconds_total", **consumer) or 0
        # Refetches back off up to 60 s: a refusal after the sends shows within that.
        refused = bool(eventually(
            lambda: (self.metric("tgin_consumer_backpressure_seconds_total", **consumer) or 0) > after_sends, 150, 1
        ))
        # The queue's counter grows on any refused batch; the box the test
        # messages go to must be the one rolled back and fetching again.
        catching = bool(eventually(lambda: refetching(self.account(), chat), 30))
        depth = self.metric("tgin_consumer_queue_depth", **consumer)
        leased = self.metric("tgin_consumer_leased", **consumer)
        conditions = {
            "queue full of leases": full,
            "a refusal after the last send": refused,
            "the targeted box refetching": catching,
            "nothing queued unleased": depth == 0 and (leased or 0) >= TIGHT_CAPACITY,
        }
        window = all(conditions.values())
        name = f"{chat}: the kill window (admitted all leased, later updates refused)"
        grew = (self.metric("tgin_consumer_backpressure_seconds_total", **consumer) or 0) - pressure0
        if window:
            CHECK(name, True, f"backpressure +{grew:.1f}s")
        else:
            CHECK.unsettled(name, "not reached: " + ", ".join(k for k, ok in conditions.items() if not ok))
        before = {t: len(self.ledger.seen(t)) for t in texts}
        self.kill()
        held = sum(1 for n in before.values() if n)
        say(f"{chat}: tgin killed with {held} of {len(texts)} messages leased and unacknowledged, the rest refused")
        return texts, before, window, undecoded0

    def verify_replay(self, chat, texts, before, window, undecoded0, finding=False):
        missing, catching = self.wait_replay(texts, chat)
        after = {t for t in texts if len(self.ledger.seen(t)) > before[t]}
        twice = {t for t in after if before[t]}
        unreadable = sum(self.ledger.undecoded.values()) - undecoded0
        name = f"{chat}: no message lost across the kill (seen before it or replayed after)"
        lost = f"{len(missing)} of {len(texts)} not seen"
        if not missing:
            CHECK(name, True, f"{len(texts)} seen")
            unsettled = None if window else "the kill window was not reached"
        elif not window:
            unsettled = f"{lost}, but the kill window was not reached"
        elif unreadable:
            unsettled = f"{lost}, and Telethon could not read {unreadable} update(s) during the phase"
        elif catching:
            unsettled = f"{lost} while the box still catches up after --catch-up-wait"
        else:
            unsettled = None
            CHECK(name, False, lost)
        if missing and unsettled:
            CHECK.unsettled(name, unsettled)
        say(f"{chat}: replayed after the restart {len(after)}; seen both before and after {len(twice)}")
        if finding:
            if unsettled:
                verdict = f"INCONCLUSIVE ({unsettled})"
            elif missing:
                verdict = f"NOT at least once: {len(missing)} of {len(texts)} posts lost"
            else:
                verdict = "at least once held for every post across this kill"
            say(f"FINDING (channel box, for the docs): {verdict}")
        decoded = all(s["decoded"] for t in texts for s in self.ledger.seen(t))
        CHECK(f"{chat}: every update of the phase decodes with Telethon (tl, users, chats)", decoded)

    def phase_replay_common(self):
        say("\nRESTART REPLAY (common box)")
        self.stop()
        self.start("tight")
        self.wait_running()
        self.drain(self.args.catch_up_wait)
        texts, before, window, undecoded0 = self.fill_and_kill("private", "p")
        self.start("tight")
        self.wait_running()
        self.verify_replay("private", texts, before, window, undecoded0)
        self.drain(self.args.catch_up_wait)
        self.delete_sent()

    def phase_channel_box(self):
        say("\nCHANNEL BOX")
        self.drain(self.args.catch_up_wait)
        texts, before, window, undecoded0 = self.fill_and_kill("channel", "c")
        self.start("normal")
        self.wait_running()
        self.verify_replay("channel", texts, before, window, undecoded0, finding=True)
        self.drain(self.args.catch_up_wait)
        self.delete_sent()

    def cleanup(self):
        """Deletes whatever the run sent and did not delete yet."""
        try:
            if self.sent:
                self.delete_sent()
        except Exception as err:  # noqa: BLE001 - say it, masked, and go on
            say(f"cleanup: the bot's deletions failed ({MASK(err)}); delete the run's messages by hand")
        if self.saved_sent and self.tgin is not None and self.tgin.poll() is None:
            try:
                f = self.tl.functions
                self.invoke(self.rpc(), f.messages.DeleteMessagesRequest(id=self.saved_sent, revoke=True))
                self.saved_sent = []
            except Exception:  # noqa: BLE001
                pass
        if self.saved_sent:
            say(f"cleanup: {len(self.saved_sent)} message(s) left in saved; delete them by hand")

    def finish(self):
        entry = self.account() or {}
        CHECK("the authorization is left logged in (account running)", entry.get("state") == "running", entry.get("state"))
        self.stop()

    def scan_logs(self):
        leaks = self.keys.leaks()
        found = Counter()
        lines = 0
        warnings = []
        for path in self.logs:
            if not path.exists():
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            lines += len(text.splitlines())
            warnings += [line for line in text.splitlines() if " WARN " in line or " ERROR " in line]
            for what, pattern in leaks:
                n = len(pattern.findall(text))
                if n:
                    found[what] += n
        CHECK(f"tgin's logs ({lines} lines) hold no secret or identifier", not found,
              ", ".join(f"{w}: {n}" for w, n in found.items()))
        if found:
            for path in self.logs:
                path.unlink(missing_ok=True)
            say("the logs held a secret and were deleted")
        say(f"tgin's logs have {len(warnings)} warning or error lines; they are in {self.workdir}")
        for line in warnings[:10]:
            say("  " + line)

    def go(self):
        try:
            self.start("normal")
            entry = self.phase_login()
            self.layer = entry["layer"]
            CHECK("the account serves one layer", isinstance(self.layer, int) and self.layer > 0, self.layer)
            taken = self.drain(self.args.catch_up_wait)
            say(f"drained {taken} updates the account caught up (counts only)")
            self.phase_updates()
            self.phase_rpc()
            self.phase_channel_warmup()
            self.phase_replay_common()
            self.phase_channel_box()
            say("\nAFTER")
            say(f"other chats: {self.ledger.summary()}")
            if self.ledger.undecoded:
                say(f"updates Telethon could not decode, by constructor: {dict(self.ledger.undecoded)}")
            self.finish()
        finally:
            self.cleanup()
            self.stop()
        self.scan_logs()
        say(f"bot messages sent: {self.bot_messages} of {self.args.max_bot_messages}")


# --- dry run ------------------------------------------------------------------


def dry_run(args):
    """Renders the configs, validates them, checks the pure helpers and the
    token pipe, prints the plan. No secret is read, no connection opened, the
    session directory is never touched."""
    say("DRY RUN: no secret read, no network, no session")
    tl, adapter = load_telethon()
    workdir = Path(tempfile.mkdtemp(prefix="tgin-mtproto-smoke-dry-"))
    configs = write_configs(workdir, args)
    say(f"configs rendered into {workdir}: {', '.join(p.name for p in configs.values())}")
    text = "".join(p.read_text(encoding="utf-8") for p in configs.values())
    CHECK("the rendered configs hold only ${env:…} references for secrets and the session",
          all(f"${{env:{v}}}" in text for v in ("TGIN_SMOKE_ADMIN_TOKEN", "TGIN_SMOKE_RPC_TOKEN", "TGIN_SMOKE_PULL_TOKEN",
                                                "TGIN_SMOKE_API_ID", "TGIN_SMOKE_API_HASH", "TGIN_SMOKE_SESSION_DIR"))
          and not re.search(r"[0-9]{6,}", text))
    if args.tgin.is_file():
        env = {k: v for k, v in os.environ.items() if not k.startswith("TGIN_SMOKE_")}
        env.update(
            TGIN_SMOKE_ADMIN_TOKEN="placeholder-admin-token",
            TGIN_SMOKE_RPC_TOKEN="placeholder-rpc-token",
            TGIN_SMOKE_PULL_TOKEN="placeholder-pull-token",
            TGIN_SMOKE_API_ID="1",
            TGIN_SMOKE_API_HASH="placeholder",
            TGIN_SMOKE_SESSION_DIR=str(workdir / "session"),
        )
        for name, path in configs.items():
            done = subprocess.run([str(args.tgin), "config", "validate", str(path)], env=env, capture_output=True, text=True,
                                  timeout=60)
            CHECK(f"tgin config validate accepts the {name} config", done.returncode == 0,
                  (done.stdout + done.stderr).strip().replace("\n", " | "))
    else:
        say(f"no tgin binary at {args.tgin}: the configs were not validated")
    self_check(tl, adapter)
    state_checks()
    relay_check()
    print_plan(args)
    say(f"session directory for the live run: {args.session_dir} (not touched)")
    return 1 if CHECK.failed or CHECK.inconclusive else 0


def relay_check():
    """run_relayed, as the login runs: the token reaches the child through the
    pipe, an id the child prints on stdout or stderr comes out masked, and a
    QR line of half blocks gets through an output stream that started as
    cp1252, as a Windows pipe or redirect does, once utf8_output (which main
    applies to the real stdout and stderr) has made it UTF-8. The child is the
    base interpreter (a venv's python.exe on Windows is a launcher whose own
    child would open the pipe, and the pipe serves its direct child only).
    What is relayed is captured here, never shown."""
    probe = secrets.token_urlsafe(24)
    fake_id = str(10**9 + secrets.randbelow(9 * 10**9))
    MASK.add(probe, fake_id)
    qr = "\u2580\u2584\u2588 \u2588\u2584\u2580"
    python = getattr(sys, "_base_executable", None) or sys.executable
    child = (
        "import hashlib, sys\n"
        "sys.stdout.reconfigure(encoding='utf-8')\n"
        "token = open(sys.argv[1]).read()\n"
        "print('digest', hashlib.sha256(token.encode()).hexdigest())\n"
        "print('token', token)\n"
        f"print({qr!r})\n"
        "print('logged in as user', sys.argv[2], flush=True)\n"
        "print('stderr user', sys.argv[2], file=sys.stderr)\n"
    )
    raw = io.BytesIO()
    shown = io.TextIOWrapper(raw, encoding="cp1252")
    utf8_output(shown)
    try:
        with contextlib.redirect_stdout(shown):
            code = run_relayed([python, "-c", child, None, fake_id], probe)
        shown.flush()
    except UnicodeEncodeError:
        return CHECK("the token pipe feeds the child; its output is relayed masked, in UTF-8", False,
                     "the QR line could not be written")
    text = raw.getvalue().decode("utf-8", errors="replace")
    lines = text.splitlines()
    digest = "digest " + hashlib.sha256(probe.encode()).hexdigest()
    ok = code == 0 and digest in lines and qr in lines
    ok &= "logged in as user <redacted>" in lines and "stderr user <redacted>" in lines and "token <redacted>" in lines
    ok &= fake_id not in text and probe not in text
    return CHECK("the token pipe feeds the child; its output is relayed masked, in UTF-8", bool(ok))


# --- main ---------------------------------------------------------------------


def budget(value):
    n = int(value)
    if not 1 <= n <= BOT_MESSAGE_CAP:
        raise argparse.ArgumentTypeError(f"must be 1 to {BOT_MESSAGE_CAP}")
    return n


def main():
    utf8_output(sys.stdout, sys.stderr)
    secrets_dir = Path.home() / ".tgin-secrets"
    parser = argparse.ArgumentParser(
        description="Check tgin's user accounts (MTProto) on the owner's account with a test bot (manual; never in CI).",
        epilog="Run with `uv run --project examples/mtproto python tests/smoke/mtproto.py`. Nothing secret is printed.",
    )
    parser.add_argument("--dry-run", action="store_true", help="render the configs, check the helpers, print the plan; offline")
    parser.add_argument("--tgin", type=Path, default=ROOT / "target" / "release" / EXE, help="the tgin binary (default: %(default)s)")
    parser.add_argument("--secrets", type=Path, default=secrets_dir, help="where the secret files are (default: %(default)s)")
    parser.add_argument("--session-dir", type=Path, help="the account's session directory (default: <secrets>/m3-account/session)")
    parser.add_argument("--api-env", type=Path, help="TELEGRAM_API_ID and TELEGRAM_API_HASH (default: <secrets>/telegram-api.env)")
    parser.add_argument("--token-file", type=Path, help="the test bot's token (default: <secrets>/bot-token)")
    parser.add_argument("--gate-env", type=Path, help="TGIN_GATE_USER and TGIN_GATE_CHANNEL (default: <secrets>/m3-gate.env)")
    parser.add_argument("--login", action="store_true",
                        help="allow a new login: from a missing session store, or when the account is logged out")
    parser.add_argument("--max-bot-messages", type=budget, default=20,
                        help=f"test-bot messages this run may send, 1 to {BOT_MESSAGE_CAP} (default: %(default)s)")
    parser.add_argument("--wait", type=int, default=120, help="seconds to wait for each expected update (default: %(default)s)")
    parser.add_argument("--catch-up-wait", type=int, default=600,
                        help="seconds the consumer may take to drain a catch-up (default: %(default)s)")
    parser.add_argument("--api-url", default="https://api.telegram.org", help="the Bot API (default: %(default)s)")
    parser.add_argument("--data-port", type=int, default=18180, help="tgin's data plane on 127.0.0.1 (default: %(default)s)")
    parser.add_argument("--admin-port", type=int, default=19190, help="tgin's admin plane on 127.0.0.1 (default: %(default)s)")
    args = parser.parse_args()
    args.session_dir = args.session_dir or args.secrets / "m3-account" / "session"
    args.api_env = args.api_env or args.secrets / "telegram-api.env"
    args.token_file = args.token_file or args.secrets / "bot-token"
    args.gate_env = args.gate_env or args.secrets / "m3-gate.env"

    try:
        if args.dry_run:
            return dry_run(args)
        print_plan(args)
        tl, adapter = load_telethon()
        if not all([self_check(tl, adapter), state_checks(), relay_check()]):
            raise Abort("an offline self-check failed; nothing was started")
        if not args.tgin.is_file():
            raise Abort(f"no tgin binary at {args.tgin}; build it (cargo build --release -p tgin) or pass --tgin")
        if not (args.session_dir / "session.redb").is_file() and not args.login:
            raise Abort(
                f"no session store at {args.session_dir}: the existing authorization is not there. "
                "Pass --session-dir, or --login to start from an empty store (a new login)"
            )
        keys = Secrets(args)
        Run(args, keys, tl, adapter).go()
    except Abort as err:
        say(f"ABORT: {err}")
        return 2
    except KeyboardInterrupt:
        say("ABORT: interrupted")
        return 2
    except Exception:  # noqa: BLE001 - print it masked, never raw
        say("ERROR:\n" + traceback.format_exc())
        return 2
    say(f"\nRESULT: {CHECK.passed} passed, {len(CHECK.failed)} failed, {len(CHECK.inconclusive)} inconclusive"
        + (": " + ", ".join(CHECK.failed + CHECK.inconclusive) if CHECK.failed or CHECK.inconclusive else ""))
    return 1 if CHECK.failed else 3 if CHECK.inconclusive else 0


if __name__ == "__main__":
    sys.exit(main())

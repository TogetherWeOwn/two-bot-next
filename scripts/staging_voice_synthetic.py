#!/usr/bin/env python3
"""Live staging voice synthetic: a dedicated test bot joins the staging creator
and the harness asserts the temp-voice room lifecycle, so no human has to join.

What it proves on the deployed staging bot (stdlib only, no third-party code):

  create     joining the creator produces a new room in the creator's category
             and moves the test bot into it
  name       the room is CREATED already carrying a template name (not the
             "<display>'s room" fallback), so no rename is needed
  position   the room sits directly below its creator among the category's
             voice channels
  owner      the room has a member overwrite for the joiner that allows View,
             Connect and Manage Channels (owner grants)
  status     a voice-channel status line is set on the room
  delete     after the test bot leaves, the room is deleted within the grace
  metrics    optional: /ops/metrics `two_bot_voice_names_total` counts the room
             as created_with_template (SKIP when no metrics token is configured)

Identity and fences (fail-closed, before any request):
  - TWO_VOICE_SYNTHETIC_BOT_TOKEN: a dedicated test bot (never the bot under
    test). The staging bot counts it as a human occupant only through the
    staging-only TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS setting.
  - Guild must be exactly TWO Staging; the live guild or anything else refuses
    with exit 2 and sends nothing.
  - Creator must be the staging "Create a Lobby" creator (the only creator).

Exit codes: 0 every check passed (or SKIP where allowed), 1 a check failed,
2 refused before contact (fence or missing configuration).

Usage:
  TWO_VOICE_SYNTHETIC_BOT_TOKEN=... python3 scripts/staging_voice_synthetic.py \\
      [--staging-url URL] [--evidence FILE]
"""

import argparse
import base64
import json
import os
import re
import socket
import ssl
import struct
import sys
import threading
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"
# Staging "Create a Lobby" creator (the only creator; "Join to Create" is retired).
STAGING_CREATOR_ID = "1546211378430345286"
STAGING_ORIGIN = "https://two-bot-next-staging"
API = "https://discord.com/api/v10"
GATEWAY = "gateway.discord.gg"
INTENTS = 1 | (1 << 7)  # GUILDS | GUILD_VOICE_STATES (no privileged intents)
VIEW, CONNECT, MANAGE = 1 << 10, 1 << 20, 1 << 4
OWNER_BITS = VIEW | CONNECT | MANAGE
VOICE_TYPES = (2, 13)
FALLBACK = re.compile(r"^.+['’]s room$")


class Refusal(Exception):
    pass


# --------------------------------------------------------------------------
# Pure verdict logic (offline-testable)
# --------------------------------------------------------------------------

def check(name, ok, detail, skip=False):
    return {"check": name, "verdict": "SKIP" if skip else ("PASS" if ok else "FAIL"), "detail": detail}


def voice_order(channels, parent_id):
    """Voice channels of one category in Discord's display order (position, id)."""
    rows = [c for c in channels if c.get("parent_id") == parent_id and c.get("type") in VOICE_TYPES]
    return sorted(rows, key=lambda c: (c.get("position", 0), int(c["id"])))


def creator_block(channels_before, creator):
    """The creator's existing rooms before the join, mirroring plan_placement:
    a creator that already has rooms groups the new one at the end of that
    block. A room is a voice channel carrying a member (type 1) overwrite that
    grants Manage Channels (the owner grant); the block is the run of such
    channels directly after the creator."""
    order = voice_order(channels_before, creator.get("parent_id"))
    ids = [c["id"] for c in order]
    if creator["id"] not in ids:
        return []
    block = []
    for channel in order[ids.index(creator["id"]) + 1:]:
        owned = any(o.get("type") == 1 and int(o.get("allow", 0)) & MANAGE
                    for o in channel.get("permission_overwrites", []))
        if not owned:
            break
        block.append(channel["id"])
    return block


def evaluate(obs):
    """Checks for one synthetic run. ``obs`` keys:

    me, creator (channel dict), room_id, room_created (CHANNEL_CREATE payload or
    None), channels (guild channels after the move), statuses (room status
    strings seen), deleted (bool), delete_seconds, grace_seconds, metrics_delta
    (int or None when unavailable), channels_before (guild channels before the
    join; the creator's existing rooms move the expected slot to their end).
    """
    out = []
    creator, room_id = obs["creator"], obs.get("room_id")
    created = obs.get("room_created") or {}
    out.append(check("create", bool(room_id) and created.get("parent_id") == creator.get("parent_id"),
                     f"room={room_id} parent={created.get('parent_id')} creator_parent={creator.get('parent_id')}"))
    name = created.get("name", "")
    out.append(check("name", bool(name) and not FALLBACK.match(name),
                     f"created name {name!r} (fallback pattern <display>'s room must not match)"))
    order = [c["id"] for c in voice_order(obs.get("channels", []), creator.get("parent_id"))]
    block = [r for r in creator_block(obs.get("channels_before", []), creator) if r != room_id]
    after = block[-1] if block else creator["id"]
    ok = room_id in order and after in order and order.index(room_id) == order.index(after) + 1
    out.append(check("position", ok, f"category voice order {order}; room must follow {after} "
                                     f"(creator {creator['id']}, its existing rooms {block})"))
    mine = [o for o in created.get("permission_overwrites", [])
            if o.get("type") == 1 and o.get("id") == obs["me"]]
    allow = int(mine[0]["allow"]) if mine else 0
    out.append(check("owner", allow & OWNER_BITS == OWNER_BITS,
                     f"joiner overwrite allow={allow} needs View|Connect|Manage Channels"))
    statuses = [s for s in obs.get("statuses", []) if s]
    out.append(check("status", bool(statuses), f"status lines seen: {statuses[:3]}"))
    out.append(check("delete", bool(obs.get("deleted")),
                     f"deleted={obs.get('deleted')} after {obs.get('delete_seconds')}s (grace {obs.get('grace_seconds')}s)"))
    delta = obs.get("metrics_delta")
    out.append(check("metrics", delta is not None and delta >= 1,
                     "no metrics token configured" if delta is None else f"created_with_template +{delta}",
                     skip=delta is None))
    return out


def fence(guild_id, creator_id, staging_url):
    if guild_id == LIVE_GUILD_ID or guild_id != STAGING_GUILD_ID:
        raise Refusal(f"guild {guild_id!r} is not TWO Staging; refusing before any request")
    if creator_id != STAGING_CREATOR_ID:
        raise Refusal(f"creator {creator_id!r} is not the staging Create a Lobby creator")
    if staging_url:
        parts = urlsplit(staging_url)
        if parts.scheme != "https" or not f"{parts.scheme}://{parts.netloc}".startswith(STAGING_ORIGIN):
            raise Refusal("staging URL must be the two-bot-next-staging workers.dev origin")


def counter(text, outcome):
    """Sum of two_bot_voice_names_total{outcome=...} samples in a Prometheus body."""
    total = 0.0
    for line in text.splitlines():
        if line.startswith("two_bot_voice_names_total{") and f'outcome="{outcome}"' in line:
            try:
                total += float(line.rsplit(" ", 1)[1])
            except ValueError:
                pass
    return total


# --------------------------------------------------------------------------
# Minimal RFC 6455 client (text frames, fragmentation, ping/pong, close)
# --------------------------------------------------------------------------

class WebSocket:
    def __init__(self, host, path, timeout=30):
        raw = socket.create_connection((host, 443), timeout=timeout)
        context = ssl.create_default_context()
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.sock = context.wrap_socket(raw, server_hostname=host)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n"
                           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                           "Sec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(1)
            if not chunk:
                raise ConnectionError("gateway closed during handshake")
            head += chunk
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError("gateway refused the websocket upgrade")
        self.lock = threading.Lock()

    def _exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("gateway closed")
            buf += chunk
        return buf

    def send(self, payload):
        data = json.dumps(payload).encode()
        mask = os.urandom(4)
        n = len(data)
        head = bytes([0x81])
        if n < 126:
            head += bytes([0x80 | n])
        elif n < 65536:
            head += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            head += bytes([0x80 | 127]) + struct.pack(">Q", n)
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        with self.lock:
            self.sock.sendall(head + mask + body)

    def recv(self):
        """Next complete text message as JSON (answers pings; None on close)."""
        parts = []
        while True:
            b1, b2 = self._exact(2)
            op, fin, n = b1 & 0x0F, b1 & 0x80, b2 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._exact(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._exact(8))[0]
            mask = self._exact(4) if b2 & 0x80 else None
            data = self._exact(n)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            if op == 0x8:
                return None
            if op == 0x9:
                with self.lock:
                    m = os.urandom(4)
                    self.sock.sendall(bytes([0x8A, 0x80 | len(data)]) + m
                                      + bytes(b ^ m[i % 4] for i, b in enumerate(data)))
                continue
            if op in (0x1, 0x0):
                parts.append(data)
                if fin:
                    return json.loads(b"".join(parts))

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# --------------------------------------------------------------------------
# Live run
# --------------------------------------------------------------------------

def rest(token, path, timeout=20):
    req = urllib.request.Request(f"{API}{path}", headers={
        "Authorization": f"Bot {token}", "User-Agent": "two-bot-voice-synthetic (staging)"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def metrics_value(staging_url, token):
    if not (staging_url and token):
        return None
    req = urllib.request.Request(f"{staging_url.rstrip('/')}/ops/metrics", headers={
        "Authorization": f"Bearer {token}", "User-Agent": "two-bot-voice-synthetic (staging)"})
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return counter(resp.read().decode(), "created_with_template")
    except (urllib.error.URLError, OSError):
        return None


class Session:
    """Gateway session: identify, heartbeat, collect the events the checks need."""

    def __init__(self, token):
        self.token, self.events, self.me = token, [], None
        self.ws = WebSocket(GATEWAY, "/?v=10&encoding=json")
        hello = self.ws.recv()
        self.interval = hello["d"]["heartbeat_interval"] / 1000
        self.seq, self.alive = None, True
        threading.Thread(target=self._beat, daemon=True).start()
        self.ws.send({"op": 2, "d": {"token": token, "intents": INTENTS,
                                     "properties": {"os": "linux", "browser": "voice-synthetic",
                                                    "device": "voice-synthetic"}}})

    def _beat(self):
        while self.alive:
            time.sleep(self.interval)
            try:
                self.ws.send({"op": 1, "d": self.seq})
            except OSError:
                return

    def pump(self, until, deadline):
        """Read dispatches until ``until(event)`` is true or the deadline passes."""
        while time.monotonic() < deadline:
            self.ws.sock.settimeout(max(0.5, deadline - time.monotonic()))
            try:
                msg = self.ws.recv()
            except socket.timeout:
                return None
            if msg is None:
                raise ConnectionError("gateway closed the session")
            if msg.get("s") is not None:
                self.seq = msg["s"]
            if msg.get("op") != 0:
                continue
            event = {"t": msg["t"], "d": msg["d"]}
            self.events.append(event)
            if msg["t"] == "READY":
                self.me = msg["d"]["user"]["id"]
            if until(event):
                return event
        return None

    def voice(self, guild_id, channel_id):
        self.ws.send({"op": 4, "d": {"guild_id": guild_id, "channel_id": channel_id,
                                     "self_mute": True, "self_deaf": True}})

    def close(self):
        self.alive = False
        self.ws.close()


def run_live(args, token):
    obs = {"grace_seconds": args.grace, "statuses": [], "deleted": False}
    before = metrics_value(args.staging_url, os.environ.get("TWO_STAGING_METRICS_TOKEN"))
    s = Session(token)
    joined = False
    try:
        if not s.pump(lambda e: e["t"] == "GUILD_CREATE" and e["d"]["id"] == args.guild, time.monotonic() + 30):
            raise RuntimeError("the test bot is not in the staging guild (no GUILD_CREATE)")
        obs["me"] = s.me
        creator = rest(token, f"/channels/{args.creator}")
        obs["creator"] = creator
        obs["channels_before"] = rest(token, f"/guilds/{args.guild}/channels")
        s.voice(args.guild, args.creator)
        joined = True
        moved = s.pump(lambda e: e["t"] == "VOICE_STATE_UPDATE" and e["d"].get("user_id") == s.me
                       and e["d"].get("channel_id") not in (None, args.creator), time.monotonic() + args.create_within)
        room_id = moved["d"]["channel_id"] if moved else None
        obs["room_id"] = room_id
        obs["room_created"] = next((e["d"] for e in s.events
                                    if e["t"] == "CHANNEL_CREATE" and e["d"]["id"] == room_id), None)
        if room_id:
            # Let the status line and any follow-up writes land, collecting events.
            s.pump(lambda e: False, time.monotonic() + args.settle)
            if obs["room_created"] is None:
                obs["room_created"] = rest(token, f"/channels/{room_id}")
            obs["channels"] = rest(token, f"/guilds/{args.guild}/channels")
            obs["statuses"] = [e["d"].get("status") for e in s.events
                               if e["t"] == "VOICE_CHANNEL_STATUS_UPDATE" and e["d"].get("id") == room_id]
        s.voice(args.guild, None)
        joined = False
        left = time.monotonic()
        if room_id:
            gone = s.pump(lambda e: e["t"] == "CHANNEL_DELETE" and e["d"]["id"] == room_id,
                          time.monotonic() + args.grace + args.delete_slack)
            obs["deleted"] = gone is not None
            obs["delete_seconds"] = round(time.monotonic() - left, 1)
    finally:
        if joined:
            try:
                s.voice(args.guild, None)
            except OSError:
                pass
        s.close()
    after = metrics_value(args.staging_url, os.environ.get("TWO_STAGING_METRICS_TOKEN"))
    obs["metrics_delta"] = None if before is None or after is None else int(after - before)
    obs.setdefault("creator", {"id": args.creator})
    obs.setdefault("channels", [])
    return obs


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--guild", default=os.environ.get("DISCORD_STAGING_GUILD_ID", STAGING_GUILD_ID))
    p.add_argument("--creator", default=STAGING_CREATOR_ID)
    p.add_argument("--staging-url", default=os.environ.get("STAGING_WORKER_URL"))
    p.add_argument("--create-within", type=float, default=20.0)
    p.add_argument("--settle", type=float, default=8.0)
    p.add_argument("--grace", type=float, default=float(os.environ.get("TWO_STAGING_EMPTY_GRACE_SECONDS", "60")))
    p.add_argument("--delete-slack", type=float, default=20.0)
    p.add_argument("--evidence", default=None)
    args = p.parse_args(argv)
    token = os.environ.get("TWO_VOICE_SYNTHETIC_BOT_TOKEN", "").strip()
    try:
        fence(args.guild, args.creator, args.staging_url)
        if not token:
            raise Refusal("TWO_VOICE_SYNTHETIC_BOT_TOKEN is not set (dedicated test bot, see docs/voice-synthetic.md)")
    except Refusal as exc:
        print(f"REFUSED: {exc}", file=sys.stderr)
        return 2
    try:
        obs = run_live(args, token)
        checks = evaluate(obs)
    except Exception as exc:  # report, never leak the token
        checks = [check("run", False, f"{type(exc).__name__}: {str(exc).replace(token, '***')}")]
        obs = {}
    for c in checks:
        print(f"{c['verdict']:4} {c['check']:9} {c['detail']}")
    if args.evidence:
        with open(args.evidence, "w") as fh:
            json.dump({"checks": checks, "room_id": obs.get("room_id"), "guild": args.guild,
                       "creator": args.creator}, fh, indent=2)
    return 1 if any(c["verdict"] == "FAIL" for c in checks) else 0


if __name__ == "__main__":
    sys.exit(main())

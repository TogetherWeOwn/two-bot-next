"""Loopback-only Discord permission fixture for the installed Bot rehearsal."""

import hmac
import json
import os
import re
import ssl
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

GUILD = "1545644954272137297"  # Compile-time receiver fence, used only as fixture data.
BOT = "111111111111111110"
MEMBER = "111111111111111111"
ROLE = "111111111111111112"
BOT_ROLE = "111111111111111113"
CHANNEL = "111111111111111114"
EVENT = "111111111111111115"
MESSAGE = "111111111111111116"
EVENT_PATH = rf"/api/v[0-9]+/guilds/{GUILD}/scheduled-events"
ROLE_PATH = f"/api/v10/guilds/{GUILD}/members/{MEMBER}/roles/{ROLE}"
MESSAGE_PATH = f"/api/v10/channels/{CHANNEL}/messages"
ADMINISTRATOR = 1 << 3
MANAGE_EVENTS = 1 << 33
CREATE_EVENTS = 1 << 44


def event_write(method, path):
    return method != "GET" and re.fullmatch(rf"{EVENT_PATH}(?:/.*)?", path) is not None


def permits_event(method, permissions, created_by_bot):
    if permissions & ADMINISTRATOR:
        return True
    if method == "POST":
        return bool(permissions & CREATE_EVENTS)
    return bool(permissions & MANAGE_EVENTS or created_by_bot and permissions & CREATE_EVENTS)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, status, body=None):
        raw = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Connection", "close")
        self.close_connection = True
        if raw:
            self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(raw)

    def record(self, operation, outcome):
        with self.server.lock, (self.server.state / "journal.jsonl").open("a") as out:
            print(json.dumps([self.command, self.path, operation, outcome]), file=out, flush=True)

    def wait_at(self, operation, phase):
        self.record(operation, phase)
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            if (self.server.state / "resume").exists():
                return
            time.sleep(0.01)
        self.record(operation, "barrier-timeout")
        raise TimeoutError("fixture barrier deadline")

    def dispatch(self):
        if not hmac.compare_digest(self.headers.get("Authorization", ""), self.server.token):
            self.reply(401, {"code": 0, "message": "Synthetic token required"})
            return
        path = self.path.split("?", 1)[0]
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 <= length <= 8192:
            self.reply(413)
            return
        body = json.loads(self.rfile.read(length)) if length else {}
        if event_write(self.command, path):
            operation = {"POST": "create", "PATCH": "cancel" if body.get("status") == 4 else "edit",
                         "DELETE": "delete"}.get(self.command)
            if operation is None:
                self.record("unknown", "unexpected")
                self.reply(405)
                return
            mode = (self.server.state / "barrier").read_text().strip()
            if mode == "before":
                self.wait_at(operation, "paused-before-permission")
            # The operator owns this state outside both Bot processes and Postgres.
            # Missing or unrecognized state leaves event writes denied.
            policy = self.server.state / "policy"
            permissions = CREATE_EVENTS | MANAGE_EVENTS if policy.exists() and policy.read_text() == "open\n" else 0
            if not permits_event(self.command, permissions, created_by_bot=True):
                self.record(operation, "denied")
                self.reply(403, {"code": 50013, "message": "Missing Permissions"})
                return
            self.record(operation, "applied")
            if mode == "after":
                self.wait_at(operation, "paused-after-effect")
                self.close_connection = True
                return
            self.reply(200, {
                "id": EVENT, "guild_id": GUILD, "name": body.get("name", "Synthetic event"),
                "description": "fixture", "status": body.get("status", 1), "channel_id": None,
                "scheduled_start_time": "2099-01-01T00:00:00Z",
            })
        elif self.command == "PUT" and path == ROLE_PATH:
            self.record("role", "applied")
            self.reply(204)
        elif self.command == "POST" and path == MESSAGE_PATH:
            if self.headers.get("Host") != "discord.com":
                self.record("announcement", "unexpected")
                self.reply(400)
                return
            self.record("announcement", "applied")
            self.reply(200, {"id": MESSAGE, "channel_id": CHANNEL})
        elif self.command == "GET":
            self.read_route(path)
        else:
            self.record("unknown", "unexpected")
            self.reply(404, {"code": 0, "message": "Unexpected fixture route"})

    def read_route(self, path):
        user = {"id": BOT, "username": "synthetic", "discriminator": "0001", "bot": True}
        guild_path = f"/api/v10/guilds/{GUILD}"
        routes = {
            "/api/v10/users/@me": user,
            guild_path: {"id": GUILD, "owner_id": "111111111111111119"},
            guild_path + "/roles": [
                {"id": GUILD, "position": 0, "permissions": "0", "managed": False},
                {"id": ROLE, "position": 1, "permissions": "0", "managed": False},
                {"id": BOT_ROLE, "position": 2, "permissions": str(1 << 28), "managed": True},
            ],
            guild_path + "/members/" + BOT: {"roles": [BOT_ROLE]},
            guild_path + "/members/" + MEMBER: {"roles": []},
            guild_path + "/scheduled-events": [],
        }
        self.reply(200 if path in routes else 404, routes.get(path, {"code": 10003}))

    do_GET = do_POST = do_PATCH = do_DELETE = do_PUT = dispatch


def main():
    state = Path("/fixture")
    with ThreadingHTTPServer(("127.0.0.1", 443), Handler) as server:
        server.state = state
        server.token = "Bot " + os.environ["SYNTHETIC_BOT_TOKEN"]
        server.lock = threading.Lock()
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.minimum_version = ssl.TLSVersion.TLSv1_2
        tls.load_cert_chain(state / "server.crt", state / "server.key")
        tls.sni_callback = lambda _sock, name, _ctx: (
            (state / "discord-sni").touch() if name == "discord.com" else ssl.ALERT_DESCRIPTION_UNRECOGNIZED_NAME
        )
        server.socket = tls.wrap_socket(server.socket, server_side=True)
        (state / "recorder-ready").touch()
        server.serve_forever()


if __name__ == "__main__":
    main()

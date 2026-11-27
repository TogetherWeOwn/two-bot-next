"""Small loopback Discord double based on the startup exchange in alive.rs."""

import base64
from contextlib import closing
from functools import partial
import hashlib
from http.client import HTTPConnection, HTTPException, RemoteDisconnected
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import socket
import struct
from threading import Event
import sys

from script_intake import decode_legacy_json, unique_object


GUILD = "222200000000000001"
APP = "1111"
TOKEN = "offline-predecessor-fixture"
APPLICATION = "/api/v10/applications/@me"
COMMANDS = f"/api/v10/applications/{APP}/guilds/{GUILD}/commands"
PORT = 8081
REST = f"http://127.0.0.1:{PORT}"
API_BASE = REST
GATEWAY = f"ws://127.0.0.1:{PORT}"
HELLO = {"op": 10, "d": {"heartbeat_interval": 45000}}
RESERVATION_HEADER = "X-Offline-Pre-Reservation"
RESERVATION = os.environ.get("OFFLINE_PRE_RESERVATION")

RELEASE = Event()
READY_SENT = Event()
REST_SYNCED = Event()
# Decode byte-capped inputs with the standard decoder's nesting limit.
decode_json = partial(decode_legacy_json, object_pairs_hook=unique_object, parse_constant=int)


def websocket_accept(key):
    # SHA-1 is the WebSocket handshake protocol constant.
    digest = hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode(),
                          usedforsecurity=False).digest()
    return base64.b64encode(digest).decode()


class Discord(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = 5

    def log_message(self, *_):
        pass

    def end_headers(self):
        if RESERVATION is not None:
            self.send_header(RESERVATION_HEADER, RESERVATION)
        super().end_headers()

    def reply(self, data, code=200):
        body = json.dumps(data, allow_nan=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/fixture":
            self.reply({"ready_sent": READY_SENT.is_set(), "rest_synced": REST_SYNCED.is_set()})
        elif self.path == "/release":
            RELEASE.set()
            self.reply({})
        elif self.headers.get("Upgrade", "").lower() == "websocket":
            self.websocket()
        else:
            routes = {
                "/api/v10/users/@me": {"id": "999", "bot": True},
                APPLICATION: {"id": APP},
                f"/api/v10/guilds/{GUILD}": {"id": GUILD, "name": "Offline fixture"},
                f"/api/v10/guilds/{GUILD}/scheduled-events": [],
                COMMANDS: [],
            }
            if self.headers.get("Authorization") != f"Bot {TOKEN}":
                self.reply({}, 401)
            elif self.path.split("?", 1)[0] in routes:
                self.reply(routes[self.path.split("?", 1)[0]])
            else:
                self.reply({}, 404)

    def do_PUT(self):
        self.close_connection = True
        if self.path != COMMANDS:
            self.reply({}, 404)
            return
        if self.headers.get("Authorization") != f"Bot {TOKEN}":
            self.reply({}, 401)
            return
        try:
            size = int(self.headers.get("Content-Length", 0))
            if not 0 < size <= 65536:
                raise ValueError
            body = self.rfile.read(size)
            if len(body) != size:
                raise ValueError
            commands = decode_json(body)
            if not isinstance(commands, list) or any(not isinstance(item, dict) for item in commands):
                raise ValueError
            self.reply(commands)
            if commands:
                REST_SYNCED.set()
        except (ValueError, RecursionError):
            self.reply({}, 400)
        except OSError:
            return

    def send_frame(self, packet):
        body = json.dumps(packet).encode()
        size = bytes([len(body)]) if len(body) < 126 else b"\x7e" + struct.pack("!H", len(body))
        self.wfile.write(b"\x81" + size + body)

    def websocket(self):
        self.close_connection = True
        key = self.headers.get("Sec-WebSocket-Key", "")
        if not key:
            self.reply({}, 400)
            return
        self.send_response_only(101)
        self.send_header("Connection", "Upgrade")
        self.send_header("Upgrade", "websocket")
        self.send_header("Sec-WebSocket-Accept", websocket_accept(key))
        self.end_headers()
        self.connection.settimeout(45)
        try:
            self.send_frame(HELLO)
            while header := self.rfile.read(2):
                opcode, size = header
                if opcode != 0x81 or not size & 0x80 or size == 0xff:
                    return
                size = struct.unpack("!H", self.rfile.read(2))[0] if size == 0xfe else size & 0x7f
                mask = self.rfile.read(4)
                data = decode_json(bytes(byte ^ mask[i % 4] for i, byte in enumerate(self.rfile.read(size))))
                if data["op"] == 1:
                    self.send_frame({"op": 11, "d": None})
                # Twilight's ConfigBuilder prefixes the inert token for IDENTIFY.
                elif data["op"] == 2 and data["d"]["token"] == f"Bot {TOKEN}" and RELEASE.wait(35):
                    self.send_frame({"op": 0, "s": 1, "t": "READY", "d": {
                        "v": 10, "user": {"id": "999", "username": "mock-bot",
                                          "discriminator": "0", "mfa_enabled": False},
                        "session_id": "offline-fixture-session", "resume_gateway_url": GATEWAY,
                        "guilds": [], "application": {"id": APP, "flags": 0},
                    }})
                    READY_SENT.set()
                else:
                    return
        except (OSError, ValueError, KeyError, IndexError, TypeError, RecursionError, struct.error):
            return


def check_peers(reservation, port=PORT):
    """Exercise both protocol listeners without releasing Gateway READY."""
    key = base64.b64encode(os.urandom(16)).decode("ascii")
    upgrade = {"Connection": "Upgrade", "Upgrade": "websocket", "Sec-WebSocket-Key": key,
               "Sec-WebSocket-Version": "13"}

    def observe(path, headers, status, payload_matches):
        try:
            with closing(HTTPConnection("127.0.0.1", port, timeout=3)) as connection:
                connection.request("GET", path, headers=headers)
                with connection.getresponse() as response:
                    return (response.status == status
                            and response.headers.get_all(RESERVATION_HEADER) == [reservation]
                            and payload_matches(response))
        except (HTTPException, OSError, ValueError, RecursionError):
            return False

    def single(response, name):
        # Refuse a repeated security-sensitive header instead of picking one copy.
        values = response.headers.get_all(name) or []
        return values[0] if len(values) == 1 else None

    def gateway_hello(response):
        connection_tokens = {token.strip().lower() for token in (single(response, "Connection") or "").split(",")}
        if ("upgrade" not in connection_tokens
                or (single(response, "Upgrade") or "").lower() != "websocket"
                or single(response, "Sec-WebSocket-Accept") != websocket_accept(key)):
            return False
        frame = response.fp.read(2)
        if len(frame) != 2 or frame[0] != 0x81 or not 0 < frame[1] <= 125:
            return False
        payload = response.fp.read(frame[1])
        return len(payload) == frame[1] and decode_json(payload) == HELLO

    def rest_application(response):
        # One declared length lets read() refuse a truncated body (IncompleteRead).
        if (single(response, "Content-Length") is None or response.length is None
                or response.length > 4096):
            return False
        return decode_json(response.read()) == {"id": APP}

    return {"gateway": observe("/?v=10&encoding=json", upgrade, 101, gateway_hello),
            "rest": observe(APPLICATION, {"Authorization": f"Bot {TOKEN}", "Connection": "close"},
                            200, rest_application)}


def request(target, path):
    port = {"mock": PORT, "bot": 8080}[target]
    connection = HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        body = response.read(65537)
        if len(body) > 65536:
            raise ValueError("fixture response exceeds the body limit")
        return response.status, decode_json(body)
    except (ConnectionError, RemoteDisconnected, TimeoutError):
        return None
    finally:
        connection.close()


def serve():
    if {name for _, name in socket.if_nameindex()} != {"lo"}:
        sys.exit("Discord fixture requires a loopback-only network namespace")
    if not RESERVATION:
        sys.exit("Discord fixture requires a reservation marker")
    ThreadingHTTPServer(("127.0.0.1", PORT), Discord).serve_forever()


if __name__ == "__main__":
    if len(sys.argv) == 1:
        serve()
    elif len(sys.argv) == 3 and sys.argv[1] == "--check-peers":
        print(json.dumps(check_peers(sys.argv[2])))

    else:
        print(json.dumps(request(*sys.argv[1:])))

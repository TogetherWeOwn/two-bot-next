"""Small loopback Discord double based on the startup exchange in alive.rs."""

import base64
from functools import partial
import hashlib
from http.client import HTTPConnection, RemoteDisconnected
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import socket
import struct
from threading import Event
import sys

from script_intake import decode_legacy_json, unique_object


GUILD = "222200000000000001"
APP = "1111"
TOKEN = "offline-predecessor-fixture"
COMMANDS = f"/api/v10/applications/{APP}/guilds/{GUILD}/commands"
GATEWAY = "ws://127.0.0.1:8081"
RELEASE = Event()
READY_SENT = Event()
# Decode byte-capped inputs with the standard decoder's nesting limit.
decode_json = partial(decode_legacy_json, object_pairs_hook=unique_object, parse_constant=int)


class Discord(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = 5

    def log_message(self, *_):
        pass

    def reply(self, data, code=200):
        body = json.dumps(data, allow_nan=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/fixture":
            self.reply({"ready_sent": READY_SENT.is_set()})
        elif self.path == "/release":
            RELEASE.set()
            self.reply({})
        elif self.headers.get("Upgrade", "").lower() == "websocket":
            self.websocket()
        else:
            routes = {
                "/api/v10/users/@me": {"id": "999", "bot": True},
                "/api/v10/applications/@me": {"id": APP},
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
        # SHA-1 is the WebSocket handshake protocol constant.
        digest = hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode(),
                              usedforsecurity=False).digest()
        self.send_response_only(101)
        self.send_header("Connection", "Upgrade")
        self.send_header("Upgrade", "websocket")
        self.send_header("Sec-WebSocket-Accept", base64.b64encode(digest).decode())
        self.end_headers()
        self.connection.settimeout(45)
        try:
            self.send_frame({"op": 10, "d": {"heartbeat_interval": 45000}})
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


def request(target, path):
    port = {"mock": 8081, "bot": 8080}[target]
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


if __name__ == "__main__":
    if len(sys.argv) == 1:
        if {name for _, name in socket.if_nameindex()} != {"lo"}:
            sys.exit("Discord fixture requires a loopback-only network namespace")
        ThreadingHTTPServer(("127.0.0.1", 8081), Discord).serve_forever()
    else:
        print(json.dumps(request(*sys.argv[1:])))

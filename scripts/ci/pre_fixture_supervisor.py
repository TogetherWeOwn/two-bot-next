#!/usr/local/bin/python3 -I
"""PID 1 for the disposable native PRE image."""

from http.server import BaseHTTPRequestHandler, HTTPServer, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pwd
import re
import signal
import subprocess
import sys
import tempfile
from threading import Thread
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ci.fixtures import predecessor_discord as discord
from ci.fixtures.predecessor_snapshots import schema_check, snapshot_sql

SHAPES = {"0473/0465": 473, "0475/0465": 475}
BOT = "/home/nonroot/two-bot"
PG = "/usr/lib/postgresql/18/bin"
PG_ENV = {"PATH": f"{PG}:/usr/bin:/bin", "LC_ALL": "C.UTF-8"}
CHECKPOINT = ("SELECT count(*) = 1 FROM gateway_sessions WHERE "
              f"guild_id = '{discord.GUILD}' AND shard_id = 0 "
              "AND session_id = 'offline-fixture-session' AND seq = 1;")


def lease(env, now_ms):
    shape = env.get("PRE_SHAPE")
    cutoff = env.get("PRE_CUTOFF_MS", "")
    start_id = env.get("PRE_START_ID", "")
    if (shape not in SHAPES or not re.fullmatch(r"[1-9][0-9]{0,15}", cutoff)
            or not re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", start_id)
            or start_id == "00000000-0000-0000-0000-000000000000"
            or not 0 < int(cutoff) - now_ms <= 3_600_000
            or env.get("DISCORD_GATEWAY_URL") != discord.GATEWAY
            or env.get("DISCORD_API_BASE") != discord.API_BASE):
        raise ValueError("invalid PRE fixture lease")
    return SHAPES[shape], int(cutoff), start_id


def run(tip, cutoff, start_id):
    postgres = pwd.getpwnam("postgres")
    root = Path(tempfile.mkdtemp(prefix="pre-"))
    os.chown(root, postgres.pw_uid, postgres.pw_gid)
    # The admin connection uses a private Unix socket. TCP uses SCRAM.
    pg_options = dict(user=postgres.pw_uid, group=postgres.pw_gid, extra_groups=[],
                      env=PG_ENV, stderr=subprocess.DEVNULL)
    data = str(root / "data")
    subprocess.run([f"{PG}/initdb", "-D", data, "--auth-local=peer", "--auth-host=scram-sha-256"],
                   **pg_options, stdout=subprocess.DEVNULL, check=True)
    db = subprocess.Popen([f"{PG}/postgres", "-D", data, "-h", "127.0.0.1", "-k", str(root)],
                          **pg_options, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)

    def sql(query):
        return subprocess.run([f"{PG}/psql", "-h", str(root), "-U", "postgres", "-d", "postgres",
                               "-XqAt", "-v", "ON_ERROR_STOP=1"],
                              **pg_options, input=query, text=True, stdout=subprocess.PIPE,
                              check=True, timeout=10).stdout.strip()

    ready = [f"{PG}/pg_isready", "-q", "-h", str(root), "-U", "postgres", "-d", "postgres"]
    while db.poll() is None and subprocess.run(ready, **pg_options, timeout=3).returncode != 0:
        time.sleep(0.1)
    sql(snapshot_sql(tip))
    check = schema_check(tip)
    if sql(check) != "t":
        raise ValueError("invalid PRE fixture schema")

    class Control(BaseHTTPRequestHandler):
        timeout = 3

        def log_message(self, *_):
            pass

        def permitted(self, path):
            return (self.path == path and self.headers.get_all("x-pre-start-id") == [start_id]
                    and time.time_ns() // 1_000_000 < cutoff)

        def do_POST(self):
            if not self.permitted("/release"):
                self.send_error(404)
                return
            discord.RELEASE.set()
            self.send_response(204)
            self.end_headers()

        def do_GET(self):
            if not self.permitted("/fixture"):
                self.send_error(404)
                return
            try:
                body = json.dumps({"ready_sent": discord.READY_SENT.is_set(),
                                   "rest_synced": discord.REST_SYNCED.is_set(),
                                   "checkpointed": sql(CHECKPOINT) == "t",
                                   "schema_matches": sql(check) == "t"}).encode("ascii")
            except (OSError, subprocess.SubprocessError):
                self.send_error(503)
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    discord.RESERVATION = start_id
    services = [ThreadingHTTPServer(("127.0.0.1", discord.PORT), discord.Discord),
                HTTPServer(("0.0.0.0", 8082), Control)]
    threads = [Thread(target=server.serve_forever, daemon=True) for server in services]
    for thread in threads:
        thread.start()
    if time.time_ns() // 1_000_000 >= cutoff:
        os._exit(0)
    bot = subprocess.Popen([BOT], user=65532, group=65532, extra_groups=[], env={
        "DISCORD_GATEWAY_URL": discord.GATEWAY, "DISCORD_API_BASE": discord.API_BASE,
        "LISTEN_ADDR": "0.0.0.0:8080", "DISCORD_TOKEN": discord.TOKEN,
        "DISCORD_APPLICATION_ID": discord.APP, "GUILD_ID": discord.GUILD, "TWO_COMMANDS_PUBLISH_ON_BOOT": "1",
        "DATABASE_URL": "postgres://offline_bot:offline-fixture@127.0.0.1/postgres", "TWO_DATABASE_TLS": "local-only",
    }, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    while db.poll() is None and bot.poll() is None and all(thread.is_alive() for thread in threads):
        if time.time_ns() // 1_000_000 >= cutoff:
            os._exit(0)
        time.sleep(0.1)
    raise ValueError("PRE fixture child exited")


def main():
    if os.getpid() != 1:
        raise ValueError("PRE fixture must be PID 1")
    now_ms = time.time_ns() // 1_000_000
    tip, cutoff, start_id = lease(os.environ, now_ms)
    # Exiting PID 1 tears down the namespace, including during initdb or a
    # blocked startup. There is no child restart and no lease renewal.
    for sig in (signal.SIGALRM, signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda *_: os._exit(0))
    signal.setitimer(signal.ITIMER_REAL, (cutoff - now_ms) / 1000)
    run(tip, cutoff, start_id)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError):
        sys.exit("PRE fixture stopped")

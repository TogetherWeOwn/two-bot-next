"""Test the dormant rehearsal without a Docker daemon or a predecessor image."""

from dataclasses import replace
from http.server import ThreadingHTTPServer
import json
import socket
import subprocess
import sys
from threading import Thread
import unittest
from unittest.mock import Mock, patch

import predecessor_container_fixture as fixture
from fixtures import predecessor_discord as discord
from fixtures.predecessor_discord import COMMANDS, Discord
from pre_target import Inventory, Resource, Target


STAMPS = {"build_revision": fixture.SOURCE, "build_id": "test-1", "deploy_id": "test.1"}
IMAGE = "sha256:" + "a" * 64
PIN = {"image_id": IMAGE, "manifest_digest": "sha256:" + "b" * 64, **STAMPS}
APP_ID = "5cb771af-3421-4223-8a06-39e061f06400"
OTHER_APP_ID = "bcf0c525-7a87-4741-9bd7-cde8a22e4cb0"


def admission(tip, scope="pre-fixture-test"):
    target = Target(scope, 123, 1_800_000_000, f"{tip:04}/0465")
    inventory = Inventory(target, None, Resource(target.container_name, APP_ID), 0, False, True)
    url = f"postgresql://pre_fixture@two-bot-next-pre-123-1800003600-{tip:04}-0465.{scope}.invalid/pre"
    return target, fixture.AppDatabaseReadback(inventory, url)


class PredecessorContainerFixtureTests(unittest.TestCase):
    def test_restart_checks_both_fixture_peers_before_bot_spawn(self):
        reservation = "offline-pre-" + "1" * 32
        foreign_reservation = "offline-pre-" + "2" * 32
        bad_peer, failure = None, None

        class Peer(Discord):
            def peer(self):
                return "REST" if self.path == discord.APPLICATION else "gateway"

            def do_GET(self):
                if self.peer() == bad_peer and failure == "missing":
                    self.close_connection = True
                    self.reply({}, 404)
                    return
                super().do_GET()

            def send_header(self, key, value):
                if self.peer() == bad_peer:
                    value = {
                        ("wrong", discord.RESERVATION_HEADER): foreign_reservation,
                        ("challenge", "Sec-WebSocket-Accept"): discord.websocket_accept("forged"),
                        ("connection", "Connection"): "close",
                    }.get((failure, key), value)
                    if failure == "truncated" and key == "Content-Length":
                        # Declare one byte more than is sent, then close.
                        self.close_connection = True
                        value = str(int(value) + 1)
                    repeat = {("duplicate", "Content-Length"): "65536", ("duplicate", "Connection"): "close",
                              ("duplicate upgrade", "Upgrade"): "h2c",
                              ("duplicate accept", "Sec-WebSocket-Accept"): discord.websocket_accept("forged")}
                    if (failure, key) in repeat:
                        # Keep the valid copy first, then repeat it with a value a
                        # joined or first-value read would still accept.
                        super().send_header(key, value)
                        value = repeat[failure, key]
                    if failure == "unframed" and key == "Content-Length":
                        # A complete body ended only by close has no declared length.
                        self.close_connection = True
                        return
                super().send_header(key, value)

            def reply(self, data, code=200):
                if self.peer() == bad_peer == "REST" and code == 200 and failure in ("payload", "oversized"):
                    if failure == "payload":
                        data = {"id": "0"}
                    else:
                        # Correct JSON padded past the 4096-byte cap.
                        body = json.dumps(data).encode() + b" " * 4096
                        self.send_response(code)
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                super().reply(data, code)

            def send_frame(self, packet):
                if self.peer() != bad_peer or packet != discord.HELLO:
                    return super().send_frame(packet)
                if failure == "payload":
                    return super().send_frame({**packet, "d": {"heartbeat_interval": 1}})
                if failure == "truncated":
                    body = json.dumps(packet).encode()
                    self.wfile.write(b"\x81" + bytes([len(body) + 1]) + body)
                    raise OSError("close after a short frame")
                return super().send_frame(packet)

        effects = []
        with patch.object(discord, "RESERVATION", reservation), ThreadingHTTPServer(("127.0.0.1", 0), Peer) as server:
            thread = Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
            thread.start()
            try:
                def docker(*args, **_):
                    if args[0] == "exec":
                        self.assertEqual(args[1:], ("fixture-mock", "python3", "-B", "/fixture.py",
                                                    "--check-peers", reservation))
                        return json.dumps(discord.check_peers(reservation, server.server_port))
                    effects.append(args)

                start = ("run", "fixture-bot")
                fixture.start_fixture_bot(docker, "fixture-mock", reservation, *start)
                stop = ("stop", "fixture-bot")
                docker(*stop)
                # Status and reservation stay valid in the later cases, so
                # only protocol or payload validation can refuse them.
                for bad_peer, failures in (("gateway", ("missing", "wrong", "challenge", "connection",
                                                        "duplicate", "duplicate upgrade", "duplicate accept",
                                                        "truncated", "payload")),
                                           ("REST", ("missing", "wrong", "truncated", "unframed", "oversized",
                                                     "duplicate", "payload"))):
                    for failure in failures:
                        with self.subTest(peer=bad_peer, failure=failure):
                            with self.assertRaisesRegex(fixture.Refused, f"{bad_peer} fixture unconfirmed"):
                                fixture.start_fixture_bot(docker, "fixture-mock", reservation, *start)
                            self.assertEqual(effects, [start, stop])
                bad_peer = None
                fixture.start_fixture_bot(docker, "fixture-mock", reservation, *start)
                self.assertEqual(effects, [start, stop, start])
            finally:
                server.shutdown()
                thread.join()

    def test_cli_help_without_tomllib(self):
        result = subprocess.run(
            [sys.executable, "-B", "-c",
             "import runpy, sys; sys.modules['tomllib'] = None; "
             "sys.argv = ['predecessor_container_fixture.py', '--help']; "
             "runpy.run_path(sys.argv[0], run_name='__main__')"],
            cwd=fixture.ROOT / "scripts/ci", capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Opt-in direct-container rehearsal", result.stdout)

    def test_mock_command_publication(self):
        for token, body, status in [(fixture.TOKEN, b'[{"name":"help"}]', b"200"),
                                    ("foreign", b"[]", b"401"), (fixture.TOKEN, b"{}", b"400"),
                                    (fixture.TOKEN, b'[{"k":1,"k":2}]', b"400"),
                                    (fixture.TOKEN, b'[{"k":NaN}]', b"400"),
                                    (fixture.TOKEN, b'[{"k":1e400}]', b"400")]:
            client, server = socket.socketpair()
            with self.subTest(status=status), client, server:
                request = (f"PUT {COMMANDS} HTTP/1.1\r\nConnection: close\r\n"
                           f"Authorization: Bot {token}\r\nContent-Length: {len(body)}\r\n\r\n").encode()
                client.sendall(request + body)
                Discord(server, None, None)
                response = client.recv(4096)
                self.assertEqual(response.split(b" ", 2)[1], status)
                if status == b"200":
                    self.assertEqual(json.loads(response.split(b"\r\n\r\n")[1]), json.loads(body))

    def test_refuses_absent_or_mismatched_image_stamps(self):
        config = {"Entrypoint": ["/home/nonroot/two-bot"],
                  "Labels": {label: STAMPS[key] for key, label in fixture.STAMPS.items()}}
        image = {"Id": IMAGE, "Os": "linux", "Architecture": "amd64", "Config": config}
        docker = Mock(side_effect=lambda *_: json.dumps([image]))
        self.assertEqual(fixture.verify_image(docker, PIN, False), STAMPS)
        for stamp in (None, "foreign-build"):
            config["Labels"][fixture.STAMPS["build_id"]] = stamp
            with self.subTest(stamp=stamp), self.assertRaisesRegex(fixture.Refused, "identity or stamps"):
                fixture.verify_image(docker, PIN, False)

    def test_bound_endpoint_refuses_foreign_app_or_database_before_mutation(self):
        target, readback = admission(473)
        inventory = readback.inventory
        url = readback.database_url
        # Give every case a compatible schema. Identity alone must stop the
        # rehearsal before the first Docker operation, even snapshot seeding.
        for case, observed, reason in (
            ("shared app", replace(readback, inventory=replace(inventory, container=Resource(
                "two-bot-next-staging", APP_ID))), "app inventory mismatch"),
            ("other app UUID", replace(readback, inventory=replace(inventory, container=Resource(
                target.container_name, OTHER_APP_ID))), "app identity mismatch"),
            ("absent app", replace(readback, inventory=replace(inventory, container=None)),
             "app identity mismatch"),
            ("other app scope", replace(readback, inventory=replace(inventory, target=replace(
                target, scope="pre-fixture-other"))), "app inventory mismatch"),
            ("unconfirmed app", replace(readback, inventory=replace(inventory, complete=False)),
             "app inventory mismatch"),
            ("shared DB", replace(readback, database_url="postgresql://pre_fixture@shared-staging.invalid/pre"),
             "database identity mismatch"),
            ("other DB scope", replace(readback, database_url=url.replace("pre-fixture-test", "pre-fixture-other")),
             "database identity mismatch"),
            ("other DB run", replace(readback, database_url=url.replace("-123-", "-124-")),
             "database identity mismatch"),
            ("other PRE shape", replace(readback, database_url=admission(475)[1].database_url),
             "database identity mismatch"),
            ("other database", replace(readback, database_url=url + "_other"), "database identity mismatch"),
            ("URL override", replace(readback, database_url=url + "?host=shared-staging.invalid"),
             "database identity mismatch"),
            ("missing readback", None, "missing isolated PRE admission or readback"),
        ):
            with self.subTest(case=case):
                docker = Mock(return_value="t")
                with self.assertRaisesRegex(fixture.Refused, reason):
                    fixture.run_bound_endpoint(docker, IMAGE, IMAGE, IMAGE, STAMPS, target=target,
                                               expected_app_id=APP_ID, readback=observed)
                docker.assert_not_called()

    def test_endpoints_and_failure_cleanup(self):
        prefixes = set()
        for tip, failure, scope in [(473, None, "pre-fixture-prodcheck"), (475, None, "pre-fixture-pooler"),
                                    (473, "stamp", "pre-fixture-test"), (475, "checkpoint", "pre-fixture-test"),
                                    (473, "setup", "pre-fixture-test"), (473, "gateway peer", "pre-fixture-test"),
                                    (475, "REST peer", "pre-fixture-test"), (475, "premature", "pre-fixture-test"),
                                    (473, "cancel", "pre-fixture-test")]:
            containers, released = {}, False

            def docker(*args, input_data=None, **_):
                nonlocal released
                if args[0] == "run":
                    name = args[args.index("--name") + 1]
                    containers[name] = args
                    self.assertIn("--rm", args)
                    self.assertIn("--pull=never", args)
                    self.assertNotIn("-p", args)
                    if name.endswith("-db"):
                        self.assertNotIn(name, prefixes)
                        prefixes.add(name)
                        self.assertIn("--network=none", args)
                        self.assertIn("--tmpfs", args)
                    else:
                        anchor = name.rsplit("-", 1)[0] + "-db"
                        self.assertIn(anchor, containers)
                        self.assertEqual(args.count(f"container:{anchor}"), 2)
                    if name.endswith("-mock"):
                        decoder = fixture.ROOT / "scripts/script_intake.py"
                        self.assertIn(f"type=bind,src={decoder},dst=/script_intake.py,readonly", args)
                    if name.endswith("-bot"):
                        self.assertIn("--restart=no", args)
                        self.assertIn(f"DISCORD_GATEWAY_URL={discord.GATEWAY}", args)
                        self.assertIn(f"DISCORD_API_BASE={discord.REST}", args)
                        self.assertEqual([arg for arg in args if arg.startswith("DATABASE_URL=")],
                                         ["DATABASE_URL=postgres://offline_bot:offline-fixture@127.0.0.1/postgres"])
                        if failure == "cancel":
                            raise KeyboardInterrupt
                        if failure in ("gateway peer", "REST peer"):
                            self.fail("bot spawn reached with an unconfirmed Discord peer")
                elif args[0] == "rm":
                    containers.pop(args[-1], None)
                elif args[0] == "ps":
                    return "\n".join(containers)
                elif args[2] == "pg_isready":
                    return "accepting"
                elif args[1] == "-i":
                    if "CREATE TABLE _sqlx_migrations" in input_data:
                        self.assertEqual("ADD COLUMN attendance_recovered_at" in input_data, tip == 475)
                        self.assertIn("INSERT INTO _two_bot_migrations VALUES (465,", input_data)
                    if "FROM gateway_sessions WHERE" in input_data:
                        return "0" if failure == "checkpoint" else "1"
                    return "f" if failure == "setup" else "t"
                elif args[-2] == "--check-peers":
                    self.assertIn(f"OFFLINE_PRE_RESERVATION={args[-1]}", containers[args[1]])
                    return json.dumps({"gateway": failure != "gateway peer", "rest": failure != "REST peer"})
                else:
                    path = args[-1]
                    if path == "/release":
                        released = True
                    status, body = {
                        "/fixture": (200, {"ready_sent": released}),
                        "/release": (200, {}),
                        "/health": (200, {"status": "ok"}),
                        "/version": (200, {"sha": fixture.SOURCE, "build_id": "test-1", "deploy_id": "test.1"}),
                        "/readyz": (200 if released or failure == "premature" else 503, {
                            **STAMPS, "deploy_id": None if failure == "stamp" else STAMPS["deploy_id"],
                            "components": [["process", "ready"], ["database", "ready"],
                                           ["gateway", "ready" if released else "starting"]],
                        }),
                    }[path]
                    return json.dumps([status, body])

            with self.subTest(tip=tip, failure=failure, scope=scope):
                target, readback = admission(tip, scope)

                def rehearse():
                    return fixture.run_bound_endpoint(docker, IMAGE, IMAGE, IMAGE, STAMPS, target=target,
                                                      expected_app_id=APP_ID, readback=readback)

                if failure == "cancel":
                    with self.assertRaises(KeyboardInterrupt):
                        rehearse()
                elif failure:
                    expected = {"stamp": "stamp mismatch", "checkpoint": "checkpoint missing",
                                "premature": "pre-READY state invalid", "setup": "snapshot readback failed",
                                "gateway peer": "gateway fixture unconfirmed", "REST peer": "REST fixture unconfirmed"}
                    with self.assertRaisesRegex(fixture.Refused, expected[failure]):
                        rehearse()
                else:
                    self.assertEqual(rehearse(), {"cutover": tip, "store": 465, "process": "live",
                                                "database": "ready", "mock_gateway": "checkpointed",
                                                "pre_ready_status": 503, "ready_status": 200})
                self.assertEqual(containers, {})

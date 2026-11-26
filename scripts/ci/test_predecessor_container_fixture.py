"""Test the dormant rehearsal without a Docker daemon or a predecessor image."""

from http.server import ThreadingHTTPServer
import json
import socket
from threading import Thread
import unittest
from unittest.mock import Mock, patch

import predecessor_container_fixture as fixture
from fixtures import predecessor_discord as discord
from fixtures.predecessor_discord import COMMANDS, Discord


STAMPS = {"build_revision": fixture.SOURCE, "build_id": "test-1", "deploy_id": "test.1"}
IMAGE = "sha256:" + "a" * 64
PIN = {"image_id": IMAGE, "manifest_digest": "sha256:" + "b" * 64, **STAMPS}


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
                if key == discord.RESERVATION_HEADER and self.peer() == bad_peer and failure == "wrong":
                    value = foreign_reservation
                super().send_header(key, value)

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
                for bad_peer in ("gateway", "REST"):
                    for failure in ("missing", "wrong"):
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

    def test_endpoints_and_failure_cleanup(self):
        prefixes = set()
        for tip, failure in [(473, None), (475, None), (473, "stamp"), (475, "checkpoint"),
                             (473, "setup"), (473, "gateway peer"), (475, "REST peer"),
                             (475, "premature"), (473, "cancel")]:
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
                    if name.endswith("-bot") and failure == "cancel":
                        raise KeyboardInterrupt
                    if name.endswith("-bot") and failure in ("gateway peer", "REST peer"):
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

            with self.subTest(tip=tip, failure=failure):
                def rehearse():
                    return fixture.run_endpoint(docker, IMAGE, IMAGE, IMAGE, STAMPS, tip)

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

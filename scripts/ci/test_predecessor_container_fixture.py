"""Test the dormant rehearsal without a Docker daemon or a predecessor image."""

from base64 import b64encode
from contextlib import redirect_stdout
from dataclasses import replace
from io import StringIO
import json
from pathlib import Path
from secrets import token_bytes
import socket
import subprocess
import sys
from subprocess import CompletedProcess
from tempfile import TemporaryDirectory
from threading import Thread
import unittest
from unittest.mock import Mock, patch

import predecessor_container_fixture as fixture
from fixtures.predecessor_discord import COMMANDS, READY_SENT, RELEASE, Discord
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

    def test_mock_gateway_accepts_twilight_identify(self):
        for token in (f"Bot {fixture.TOKEN}", fixture.TOKEN, "Bot foreign"):
            client, server = socket.socketpair()
            with self.subTest(token=token), client, server, client.makefile("rb") as inbound:
                worker = Thread(target=Discord, args=(server, None, None), daemon=True)
                client.settimeout(2)
                RELEASE.set()
                worker.start()
                try:
                    nonce = b64encode(token_bytes(16))
                    client.sendall(b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                                   b"Sec-WebSocket-Key: " + nonce + b"\r\n\r\n")
                    self.assertIn(b"101", inbound.readline())
                    while inbound.readline() not in (b"\r\n", b""):
                        pass
                    self.assertEqual(inbound.read(1), b"\x81")
                    self.assertEqual(json.loads(inbound.read(inbound.read(1)[0]))["op"], 10)
                    payload = json.dumps({"op": 2, "d": {"token": token}}).encode()
                    client.sendall(bytes([0x81, 0x80 | len(payload)]) + b"\0" * 4 + payload)
                    if token == f"Bot {fixture.TOKEN}":
                        self.assertEqual(inbound.read(2), b"\x81\x7e")
                        length = int.from_bytes(inbound.read(2), "big")
                        self.assertEqual(json.loads(inbound.read(length))["t"], "READY")
                    else:
                        client.shutdown(socket.SHUT_WR)
                        worker.join(2)
                        self.assertFalse(worker.is_alive())
                        self.assertFalse(READY_SENT.is_set())
                finally:
                    client.close()
                    inbound.close()
                    worker.join(2)
                    RELEASE.clear()
                    READY_SENT.clear()

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
        docker = Mock(side_effect=lambda *_, **__: json.dumps([image]))
        self.assertEqual(fixture.verify_image(docker, PIN, False), (IMAGE, STAMPS))
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

    def test_requires_admitted_manifest_on_containerd(self):
        config = {"Entrypoint": ["/home/nonroot/two-bot"],
                  "Labels": {label: STAMPS[key] for key, label in fixture.STAMPS.items()}}
        manifest = PIN["manifest_digest"]
        image = {"Id": manifest, "Os": "linux", "Architecture": "amd64", "Config": config}
        def inspect(_command, _action, reference, *, missing_ok=False):
            if reference == IMAGE:
                self.assertTrue(missing_ok)
                return ""
            self.assertEqual(reference, manifest)
            return json.dumps([image])

        docker = Mock(side_effect=inspect)
        for descriptor in (None, IMAGE, manifest):
            image["Descriptor"] = {"digest": descriptor}
            with self.subTest(descriptor=descriptor):
                if descriptor == manifest:
                    self.assertEqual(fixture.verify_image(docker, PIN, False), (manifest, STAMPS))
                else:
                    with self.assertRaisesRegex(fixture.Refused, "identity or stamps"):
                        fixture.verify_image(docker, PIN, False)

    def test_cli_rehearses_the_admitted_manifest_when_config_id_is_unknown(self):
        manifest = PIN["manifest_digest"]
        image = {"Id": manifest, "Descriptor": {"digest": manifest}, "Os": "linux",
                 "Architecture": "amd64", "Config": {"Entrypoint": ["/home/nonroot/two-bot"],
                     "Labels": {label: STAMPS[key] for key, label in fixture.STAMPS.items()}}}
        inspected = []

        def docker(command, **_):
            self.assertEqual(command[5:7], ["image", "inspect"])
            reference, = command[7:]
            inspected.append(reference)
            if reference == IMAGE:
                return CompletedProcess(command, 1, "", f"Error response from daemon: No such image: {IMAGE}")
            self.assertEqual(reference, manifest)
            return CompletedProcess(command, 0, json.dumps([image]), "")

        with TemporaryDirectory() as directory:
            pin = Path(directory) / "pin.json"
            pin.write_text(json.dumps(PIN))
            args = ["fixture", "--pin", str(pin), "--postgres", IMAGE, "--mock-image", IMAGE]
            output = StringIO()
            with (patch.object(fixture.sys, "argv", args), patch.object(fixture.signal, "signal"),
                  patch.object(fixture.subprocess, "run", side_effect=docker),
                  patch.object(fixture, "verify_support"), patch.object(fixture, "run_endpoint") as rehearse,
                  redirect_stdout(output)):
                rehearse.side_effect = [{"cutover": 473}, {"cutover": 475}]
                fixture.main()

        self.assertEqual(inspected, [IMAGE, manifest])
        self.assertEqual([call.args[1:] for call in rehearse.call_args_list],
                         [(manifest, IMAGE, IMAGE, STAMPS, tip) for tip in (473, 475)])
        self.assertEqual(json.loads(output.getvalue())["docker_image_id"], manifest)

    def test_endpoints_and_failure_cleanup(self):
        prefixes = set()
        for tip, failure, scope in [(473, None, "pre-fixture-prodcheck"), (475, None, "pre-fixture-pooler"),
                                    (473, "stamp", "pre-fixture-test"), (475, "checkpoint", "pre-fixture-test"),
                                    (473, "setup", "pre-fixture-test"), (475, "premature", "pre-fixture-test"),
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
                        self.assertEqual([arg for arg in args if arg.startswith("DATABASE_URL=")],
                                         ["DATABASE_URL=postgres://offline_bot:offline-fixture@127.0.0.1/postgres"])
                        if failure == "cancel":
                            raise KeyboardInterrupt
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
                                "premature": "pre-READY state invalid", "setup": "snapshot readback failed"}
                    with self.assertRaisesRegex(fixture.Refused, expected[failure]):
                        rehearse()
                else:
                    self.assertEqual(rehearse(), {"cutover": tip, "store": 465, "process": "live",
                                                "database": "ready", "mock_gateway": "checkpointed",
                                                "pre_ready_status": 503, "ready_status": 200})
                self.assertEqual(containers, {})

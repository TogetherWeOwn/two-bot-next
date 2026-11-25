"""Test the dormant rehearsal without a Docker daemon or a predecessor image."""

from dataclasses import replace
import json
import socket
import unittest
from unittest.mock import Mock

import predecessor_container_fixture as fixture
from fixtures.predecessor_discord import COMMANDS, Discord
from fixtures.predecessor_snapshots import schema_check
from pre_start_stop import Endpoint, ProbeReply
from test_pre_start_stop import PreFixture, TARGET


STAMPS = {"build_revision": fixture.SOURCE, "build_id": "test-1", "deploy_id": "test.1"}
IMAGE = "sha256:" + "a" * 64
PIN = {"image_id": IMAGE, "manifest_digest": "sha256:" + "b" * 64, **STAMPS}


class PredecessorContainerFixtureTests(unittest.TestCase):
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
        for tip, post_start_tip, failure in [
            (473, 473, None), (475, 475, None),
            (473, 475, "ledger"), (475, 473, "ledger"),
            (473, 473, "stamp"), (475, 475, "checkpoint"),
            (473, 473, "setup"), (475, 475, "premature"), (473, 473, "cancel"),
        ]:
            containers, released = {}, False
            pre = PreFixture(replace(TARGET, shape=f"{tip:04d}/0465"))
            controller = pre.controller(pre.authorize)
            deployment = None
            trace = []

            def docker(*args, input_data=None, **_):
                nonlocal released, deployment
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
                        self.assertEqual(args[-1], IMAGE)
                        if failure == "cancel":
                            raise KeyboardInterrupt
                        # The private adapter stands in for completed scoped
                        # upload and ownership bootstrap.
                        deployment = controller.start(pre.upload)
                        self.assertEqual(deployment, pre.deployment)
                        trace.append(("started", deployment))
                elif args[0] == "rm":
                    containers.pop(args[-1], None)
                elif args[0] == "ps":
                    return "\n".join(containers)
                elif args[2] == "pg_isready":
                    return "accepting"
                elif args[1] == "-i":
                    self.assertIn(args[2], containers)
                    self.assertTrue(args[2].endswith("-db"))
                    if "CREATE TABLE _sqlx_migrations" in input_data:
                        self.assertEqual("ADD COLUMN attendance_recovered_at" in input_data, tip == 475)
                        self.assertIn("INSERT INTO _two_bot_migrations VALUES (465,", input_data)
                        return ""
                    if "FROM gateway_sessions WHERE" in input_data:
                        return "0" if failure == "checkpoint" else "1"
                    if "SELECT NOT EXISTS (SELECT FROM gateway_sessions)" in input_data:
                        return "t"
                    trace.append(("ledger", deployment, input_data))
                    observed_tip = tip if deployment is None else post_start_tip
                    return "t" if failure != "setup" and input_data == schema_check(observed_tip) else "f"
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
                    if path in ("/health", "/readyz"):
                        pre.reply = ProbeReply(status, pre.deployment.worker_version, json.dumps(body).encode())
                        reply = controller.probe(deployment, Endpoint(path))
                        trace.append((Endpoint(path), deployment, reply))
                        return json.dumps([reply.status, json.loads(reply.body)])
                    return json.dumps([status, body])

            with self.subTest(tip=tip, post_start_tip=post_start_tip, failure=failure):
                def rehearse():
                    return fixture.run_endpoint(docker, IMAGE, IMAGE, IMAGE, STAMPS, tip)

                if failure == "cancel":
                    with self.assertRaises(KeyboardInterrupt):
                        rehearse()
                elif failure:
                    expected = {"ledger": "^PRE snapshot changed during startup$",
                                "stamp": "stamp mismatch", "checkpoint": "checkpoint missing",
                                "premature": "pre-READY state invalid", "setup": "snapshot readback failed"}
                    with self.assertRaisesRegex(fixture.Refused, expected[failure]):
                        rehearse()
                else:
                    self.assertEqual(rehearse(), {"cutover": tip, "store": 465, "process": "live",
                                                "database": "ready", "mock_gateway": "checkpointed",
                                                "pre_ready_status": 503, "ready_status": 200})
                if failure in (None, "ledger"):
                    self.assertEqual([(event, app) for event, app, *_ in trace],
                                     [("ledger", None), ("started", deployment), (Endpoint.HEALTH, deployment),
                                      (Endpoint.READINESS, deployment), (Endpoint.READINESS, deployment),
                                      ("ledger", deployment)])
                    self.assertEqual(trace[0][2], trace[-1][2])
                    ready = trace[-2][2]
                    self.assertEqual((ready.status, ready.worker_version), (200, deployment.worker_version))
                    report = json.loads(ready.body)
                    self.assertEqual({key: report[key] for key in STAMPS}, STAMPS)
                    self.assertEqual(dict(report["components"]),
                                     {"process": "ready", "database": "ready", "gateway": "ready"})
                self.assertEqual(containers, {})

"""Test the dormant rehearsal without a Docker daemon or a predecessor image."""

import json
import unittest
from unittest.mock import Mock

import predecessor_container_fixture as fixture


STAMPS = {"build_revision": fixture.SOURCE, "build_id": "test-1", "deploy_id": "test.1"}
IMAGE = "sha256:" + "a" * 64
PIN = {"image_id": IMAGE, "manifest_digest": "sha256:" + "b" * 64, **STAMPS}


class PredecessorContainerFixtureTests(unittest.TestCase):
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
                             (473, "setup"),
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
                    if name.endswith("-bot") and failure == "cancel":
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

            with self.subTest(tip=tip, failure=failure):
                def rehearse():
                    return fixture.run_endpoint(docker, IMAGE, IMAGE, IMAGE, STAMPS, tip)

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

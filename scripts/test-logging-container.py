#!/usr/bin/env python3
"""Token-free Docker image smoke; only the container this script creates is removed."""

import json
import re
import subprocess
import sys
import time
import unittest
import uuid


REQUIRED = {"gateway_parked", "http_listening", "shutdown_started", "shutdown_completed"}


def validate(text):
    lines = text.splitlines()
    if not lines:
        raise AssertionError("container produced no logs")
    names = set()
    run_id = None
    for number, line in enumerate(lines, 1):
        event = json.loads(line)  # Blank/plain-text lines fail too.
        if not isinstance(event, dict):
            raise AssertionError(f"line {number} is not an object")
        for key in ("ts", "level", "msg", "target"):
            if not isinstance(event.get(key), str):
                raise AssertionError(f"line {number} missing string {key}")
        if not re.fullmatch(r"[a-z][a-z0-9_]*", event["msg"]):
            raise AssertionError(f"line {number} has an unstable msg")
        if not isinstance(event.get("run_id"), str) or not event["run_id"]:
            raise AssertionError(f"line {number} ({event['msg']}) missing string run_id")
        if run_id is None:
            run_id = event["run_id"]
        elif event["run_id"] != run_id:
            raise AssertionError(f"line {number} ({event['msg']}) changed run_id")
        names.add(event["msg"])
    if not REQUIRED <= names:
        raise AssertionError(f"missing lifecycle names: {sorted(REQUIRED - names)}")
    return len(lines)


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True, stderr=subprocess.STDOUT, timeout=45)


def smoke(image):
    name = "two-bot-json-smoke-" + uuid.uuid4().hex[:12]
    created = False
    try:
        # No env-file, mounts, published ports, token, database, or network access.
        docker("create", "--name", name, "--network", "none", image)
        created = True
        # Take the binary path from the image, not a hard-coded home directory.
        binary = docker("inspect", "--format", "{{index .Config.Entrypoint 0}}", name).strip()
        docker("start", name)
        deadline = time.monotonic() + 30
        while True:
            if docker("inspect", "--format", "{{.State.Running}}", name).strip() != "true":
                raise AssertionError("token-free container exited before healthcheck")
            probe = subprocess.run(
                ["docker", "exec", name, binary, "--healthcheck"],
                capture_output=True,
                timeout=5,
            )
            if probe.returncode == 0:
                break
            if time.monotonic() >= deadline:
                raise AssertionError("token-free container never became healthy")
            time.sleep(0.2)
        docker("stop", "--time", "10", name)  # SIGTERM + graceful HTTP drain.
        exit_code = docker("inspect", "--format", "{{.State.ExitCode}}", name).strip()
        if exit_code != "0":
            raise AssertionError(f"container exited {exit_code}, expected graceful exit 0")
        text = docker("logs", name)  # Combined stdout and stderr, every line checked.
        count = validate(text)
        print(f"PASS: {count} JSON lines, token-free healthcheck and SIGTERM lifecycle")
    finally:
        if created:
            docker("rm", "--force", name)


class ParserTests(unittest.TestCase):
    def fixture(self):
        return "\n".join(
            json.dumps({"ts": "2026-09-30T12:00:00Z", "level": "info", "msg": name, "target": "two_bot", "run_id": "test-run"})
            for name in sorted(REQUIRED)
        )

    def test_valid(self):
        self.assertEqual(validate(self.fixture()), 4)

    def test_plaintext_and_blank_lines_rejected(self):
        for extra in ("not json\n", "\n", "null\n", "[]\n"):
            with self.assertRaises((AssertionError, json.JSONDecodeError)):
                validate(extra + self.fixture())

    def test_missing_or_nonstring_msg_rejected(self):
        for value in (None, 1, {}, "unstable prose"):
            line = json.dumps({"ts": "x", "level": "info", "target": "x", "msg": value})
            with self.assertRaises(AssertionError):
                validate(line + "\n" + self.fixture())

    def test_missing_nonstring_and_changed_run_id_rejected(self):
        for value in (None, 1, "", "different-run"):
            events = [json.loads(line) for line in self.fixture().splitlines()]
            events[-1]["run_id"] = value
            with self.assertRaisesRegex(AssertionError, "run_id"):
                validate("\n".join(json.dumps(event) for event in events))
        events[-1].pop("run_id")
        with self.assertRaisesRegex(AssertionError, "run_id"):
            validate("\n".join(json.dumps(event) for event in events))

    def test_empty_and_incomplete_output_rejected(self):
        for text in ("", self.fixture().splitlines()[0]):
            with self.assertRaises(AssertionError):
                validate(text)


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        unittest.main(argv=[sys.argv[0]])
    elif len(sys.argv) == 2:
        smoke(sys.argv[1])
    else:
        sys.exit("usage: test-logging-container.py IMAGE | --self-test")

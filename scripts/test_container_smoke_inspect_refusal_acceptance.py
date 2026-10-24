"""Protected acceptance: container-smoke refuses malformed inspect output.

The four ``docker inspect`` chains (PID 1 identity, health-wait, health-status
and shutdown) share classified shape refusals. Non-JSON replies use the
layered JSON guard; JSON-shaped failures keep the existing per-case vocabulary.
Missing State fields use the fixed message for the field that call site needs.

The refusal surfaces as ``RuntimeError`` from ``smoke()``/``pid1_uids()`` and
as ``SystemExit("FAIL container smoke: <reason>")`` from ``main()`` (nonzero
exit, no traceback, no echo of the daemon reply).

A ``State`` without ``Pid`` keeps ``"container PID 1 has no host process"``.
A present ``Health: {}`` is not a shape refusal: without a healthy Status it
must keep waiting and eventually refuse with the documented 30s timeout.
Anything else (a traceback, parser text, an echoed key) fails.

Passing controls pin the valid-input behavior byte-for-byte (same verdict
lines, same PID 1 identity, same pre-existing value refusals) so the fix
cannot trade one refusal for another.

Offline only: ``docker`` and HTTP are mocked; no daemon, no network.
Builder must not edit this file; build on the pushed head.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke_inspect_refusal_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import json
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "container_smoke", Path(__file__).with_name("container-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


INVALID_JSON = "docker inspect output is not valid JSON"
NO_ENTRIES = "docker inspect returned no entries"
NON_OBJECT_ENTRY = "docker inspect entry must be a JSON object"
NO_STATE = "docker inspect entry has no State"
NO_RUNNING = "docker inspect State has no Running"
NO_HEALTH = "docker inspect entry has no Health"
NO_OOM = "docker inspect State has no OOMKilled"
HEALTH_TIMEOUT = "Docker HEALTHCHECK did not become healthy within 30s"
NO_HOST_PROCESS = "container PID 1 has no host process"
TOP_MISSING_PID1 = "docker top did not report PID 1"
CANARY = "INSPECT-CANARY-NEVER-ECHO-7Q3Z"

# Structural failures: no call site can use these, whatever it reads.
STRUCTURAL_MALFORMED = (
    ("not json " + CANARY, INVALID_JSON),
    ("", INVALID_JSON),
    (b"\xff" + CANARY.encode(), INVALID_JSON),
    ('{"State": {"Pid": 4242}}', NO_ENTRIES),
    ("[]", NO_ENTRIES),
    (json.dumps([CANARY]), NON_OBJECT_ENTRY),
    ("[{}]", NO_STATE),
    ('[{"State": null}]', NO_STATE),
    ('[{"State": "Running"}]', NO_STATE),
    ('[{"State": []}]', NO_STATE),
    ('{"State": {}, "note": "' + CANARY + '"}', NO_ENTRIES),
)

EMPTY_STATE = json.dumps([{"State": {}, "note": CANARY}])
MISSING_HEALTH = (
    (json.dumps([{"State": {"Running": True, "note": CANARY}}]), NO_HEALTH),
    (json.dumps([{"State": {"Running": True, "Health": None}}]), NO_HEALTH),
)
EMPTY_HEALTH = json.dumps([{"State": {
    "Running": True, "Health": {}, "note": CANARY}}])
MISSING_OOM = ((json.dumps([{"State": {"Running": False, "note": CANARY}}]), NO_OOM),)
MISSING_PID = (json.dumps([{"State": {"Running": True, "note": CANARY}}]),)

VALID_STATE = json.dumps([{"State": {
    "Running": True, "OOMKilled": False, "Pid": 4242,
    "Health": {"Status": "healthy"},
}}])

PARKED_JOB = {"parked": True, "running": False, "last_start": None,
              "last_success": None, "last_error_class": None,
              "consecutive_failures": 0}
JOB_NAMES = (
    "counter", "rank", "scheduled_events", "settings", "presence_probe",
    "community_scorecard", "inactivity", "audit_retry",
    "self_role_recovery", "scheduled_messages", "feeds",
    "member_unban_sweep", "retention", "lfg_sweep",
)


def parked_body():
    return {
        "components": [["process", "ready"], ["gateway", "down"],
                       ["database", "down"], ["token_invalid", "ready"]],
        "jobs": {name: dict(PARKED_JOB) for name in JOB_NAMES},
    }


def tar_of(path, data, mode=0o644):
    """The archive ``docker cp CONTAINER:PATH -`` writes for one file."""
    import tarfile

    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as tar:
        info = tarfile.TarInfo(Path(path).name)
        info.size, info.mode = len(data), mode
        tar.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


class SmokeFixture:
    """Minimal valid docker surface; inspect replies are scripted."""

    def __init__(self, inspect_outputs=(), *, repeat_last_inspect=False):
        self.calls = []
        self.inspect_outputs = list(inspect_outputs)
        self.repeat_last_inspect = repeat_last_inspect
        self.image_id = "sha256:" + "a" * 64
        self.stopped = False

    def __call__(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        output, code = "", 0
        if args[:2] == ("image", "inspect"):
            output = json.dumps([{
                "Id": self.image_id, "Size": 140 * smoke.MIB, "Config": {
                    "User": "65532:65532",
                    "Healthcheck": {"Test": ["CMD", smoke.BINARY, "--healthcheck"]},
                }}])
        elif args[0] == "history":
            output = f"{10 * smoke.MIB - 4096}\n4096\n0\n"
        elif args[0] == "create":
            output = "f" * 64 + "\n"
        elif args[0] == "cp":
            path = args[2].partition(":")[2]
            if path == smoke.BINARY:
                output = tar_of(path, bytes(7 * smoke.MIB), 0o755)
            else:
                output = tar_of(
                    path,
                    b"-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----\n",
                    0o644)
        elif args[0] == "port":
            output = "127.0.0.1:32768\n"
        elif args[0] == "inspect":
            if self.inspect_outputs:
                if self.repeat_last_inspect and len(self.inspect_outputs) == 1:
                    output = self.inspect_outputs[0]
                else:
                    output = self.inspect_outputs.pop(0)
            else:
                output = json.dumps([{"State": {
                    "Running": not self.stopped, "OOMKilled": False,
                    "Pid": 4242, "Health": {"Status": "healthy"}}}])
        elif args[0] == "top":
            output = ("PID   RUID   EUID   SUID   FSUID\n"
                      "4300   0   0   0   0\n"
                      "4242   65532   65532   65532   65532\n")
        elif args[0] == "exec":
            code = 0
        elif args[0] == "run" and "--healthcheck" in args:
            code = 1
        elif args[0] == "wait":
            self.stopped = True
            output = "0"
        return subprocess.CompletedProcess(args, code, output, "")


def make_http(health_then):
    state = {"n": 0}

    def http(url):
        if url.endswith("/readyz"):
            return (503, parked_body())
        state["n"] += 1
        if state["n"] <= len(health_then):
            return health_then[state["n"] - 1]
        return (200, {"status": "ok"})

    return http


class InspectRefusalAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.clock = 0

    def tick(self):
        self.clock += 1
        return self.clock

    def run_smoke(self, fixture, health_then=()):
        with patch.object(smoke, "docker", fixture), \
                patch.object(smoke, "http_response", side_effect=make_http(health_then)), \
                patch.object(smoke.time, "monotonic", side_effect=self.tick), \
                patch.object(smoke.time, "sleep"), \
                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            smoke.smoke("two-bot:fixture")
        return output.getvalue()

    def assert_inspect_refusal(self, fixture, health_then, label, reason):
        with self.assertRaises(RuntimeError,
                               msg=f"malformed inspect accepted in case {label}") as caught:
            self.run_smoke(fixture, health_then)
        self.assertEqual(str(caught.exception), reason,
                         msg=f"malformed inspect mis-refused in case {label}")
        self.assertNotIn(CANARY, str(caught.exception),
                         msg=f"daemon reply echoed in case {label}")
        self.assertNotIn("Traceback", str(caught.exception))

    # --- PID 1 identity --------------------------------------------------

    def test_pid1_structural_inspect_refuses_with_fixed_vocabulary(self):
        for bad, reason in STRUCTURAL_MALFORMED + ((EMPTY_STATE, NO_HOST_PROCESS),):
            with self.subTest(stdout=bad[:40]):
                def docker(*args, **kwargs):
                    return subprocess.CompletedProcess(args, 0, bad, "")

                with patch.object(smoke, "docker", docker):
                    with self.assertRaises(RuntimeError) as caught:
                        smoke.pid1_uids("two-bot-smoke-fixture")
                self.assertEqual(str(caught.exception), reason)
                self.assertNotIn(CANARY, str(caught.exception))
                self.assertNotIn("Traceback", str(caught.exception))

    def test_pid1_missing_pid_refuses_without_echo_or_traceback(self):
        for bad in MISSING_PID:
            with self.subTest(stdout=bad[:40]):
                def docker(*args, **kwargs):
                    return subprocess.CompletedProcess(args, 0, bad, "")

                with patch.object(smoke, "docker", docker):
                    with self.assertRaises(RuntimeError) as caught:
                        smoke.pid1_uids("two-bot-smoke-fixture")
                self.assertEqual(str(caught.exception), NO_HOST_PROCESS)
                self.assertNotIn(CANARY, str(caught.exception))
                self.assertNotIn("Traceback", str(caught.exception))

    def test_pid1_structural_refusal_echoes_no_daemon_text(self):
        bad = "not json " + CANARY
        with patch.object(smoke, "docker",
                           lambda *a, **k: subprocess.CompletedProcess(a, 0, bad, "")):
            with self.assertRaises(RuntimeError) as caught:
                smoke.pid1_uids("two-bot-smoke-fixture")
        self.assertEqual(str(caught.exception), INVALID_JSON)
        self.assertNotIn(CANARY, str(caught.exception))
        self.assertNotIn("Traceback", str(caught.exception))

    # --- health-wait -----------------------------------------------------

    def test_health_wait_malformed_inspect_refuses_with_fixed_vocabulary(self):
        for bad, reason in STRUCTURAL_MALFORMED + ((EMPTY_STATE, NO_RUNNING),):
            with self.subTest(stdout=bad[:40]):
                fixture = SmokeFixture([bad])
                self.assert_inspect_refusal(
                    fixture, [(500, {})], f"health-wait:{bad[:20]}", reason)
                self.assertEqual(fixture.calls[-1][0][:2], ("rm", "--force"))

    # --- health-status ---------------------------------------------------

    def test_health_status_malformed_inspect_refuses_with_fixed_vocabulary(self):
        cases = (STRUCTURAL_MALFORMED + ((EMPTY_STATE, NO_RUNNING),)
                 + MISSING_HEALTH + ((EMPTY_HEALTH, HEALTH_TIMEOUT),))
        for bad, reason in cases:
            with self.subTest(stdout=bad[:40]):
                # Health answers at once; PID 1 consumes the valid reply, then
                # health-status reads the malformed one. Repeat it so Health:{}
                # cannot exhaust the script and fall back to a healthy reply.
                fixture = SmokeFixture([VALID_STATE, bad], repeat_last_inspect=True)
                self.assert_inspect_refusal(fixture, [], f"health-status:{bad[:20]}", reason)
                self.assertEqual(fixture.calls[-1][0][:2], ("rm", "--force"))
                if reason == HEALTH_TIMEOUT:
                    self.assertEqual(fixture.inspect_outputs, [bad])
                    self.assertGreater(
                        sum(args[0] == "inspect" for args, _ in fixture.calls), 2)

    # --- shutdown --------------------------------------------------------

    def test_shutdown_malformed_inspect_refuses_with_fixed_vocabulary(self):
        cases = STRUCTURAL_MALFORMED + ((EMPTY_STATE, NO_RUNNING),) + MISSING_OOM
        for bad, reason in cases:
            with self.subTest(stdout=bad[:40]):
                fixture = SmokeFixture([VALID_STATE, VALID_STATE, bad])
                self.assert_inspect_refusal(fixture, [], f"shutdown:{bad[:20]}", reason)
                self.assertEqual(fixture.calls[-1][0][:2], ("rm", "--force"))

    # --- CLI surface: nonzero exit, no traceback ------------------------------

    def test_main_reports_inspect_refusal_without_traceback(self):
        for bad, reason in STRUCTURAL_MALFORMED + ((EMPTY_STATE, NO_RUNNING),):
            with self.subTest(stdout=bad[:40]):
                fixture = SmokeFixture([bad])
                with patch.object(smoke, "docker", fixture), \
                        patch.object(smoke, "http_response",
                                     side_effect=make_http([(500, {})])), \
                        patch.object(smoke.time, "monotonic", side_effect=self.tick), \
                        patch.object(smoke.time, "sleep"), \
                        patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                        patch.object(sys, "argv", ["container-smoke.py", "two-bot:fixture"]), \
                        contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaises(SystemExit) as caught:
                        smoke.main()
                self.assertEqual(caught.exception.code,
                                 "FAIL container smoke: " + reason)
                self.assertNotIn("Traceback", str(caught.exception.code))
                self.assertNotIn(CANARY, str(caught.exception.code))
                self.assertEqual(fixture.calls[-1][0][:2], ("rm", "--force"))

    # --- passing controls: valid behavior byte-identical -----------------------

    def test_control_valid_pid1_identity_unchanged(self):
        fixture = SmokeFixture()
        with patch.object(smoke, "docker", fixture):
            self.assertEqual(smoke.pid1_uids("two-bot-smoke-fixture"), [65532] * 4)

    def test_control_valid_smoke_verdicts_unchanged(self):
        output = self.run_smoke(SmokeFixture())
        for line in (
            "PASS runtime trust bundle contains readable PEM certificate data",
            "PASS /health 200; /readyz 503 parked; non-root PID 1; "
            "--healthcheck 0; Docker HEALTHCHECK healthy",
            "PASS --healthcheck exits 1 without a listening server",
            "PASS SIGTERM exits 0",
            "no OOM",
        ):
            self.assertIn(line, output)

    def test_control_unprovable_pid_keeps_existing_refusal(self):
        for pid in (0, -1, None, "4242", 4.5, [4242]):
            with self.subTest(pid=pid):
                bad = json.dumps([{"State": {
                    "Running": True, "OOMKilled": False, "Pid": pid,
                    "Health": {"Status": "healthy"}}}])

                def docker(*args, **kwargs):
                    if args[0] == "top":
                        return SmokeFixture()(("top",), **kwargs)
                    return subprocess.CompletedProcess(args, 0, bad, "")

                with patch.object(smoke, "docker", docker):
                    with self.assertRaisesRegex(
                            RuntimeError, "^" + re.escape(NO_HOST_PROCESS) + "$"):
                        smoke.pid1_uids("two-bot-smoke-fixture")

    def test_control_top_mismatch_keeps_existing_refusal(self):
        bad = json.dumps([{"State": {
            "Running": True, "OOMKilled": False, "Pid": 9999,
            "Health": {"Status": "healthy"}}}])
        fixture = SmokeFixture()

        def docker(*args, **kwargs):
            if args[0] == "inspect":
                return subprocess.CompletedProcess(args, 0, bad, "")
            return fixture(*args, **kwargs)

        with patch.object(smoke, "docker", docker):
            with self.assertRaisesRegex(
                    RuntimeError, "^" + re.escape(TOP_MISSING_PID1) + "$"):
                smoke.pid1_uids("two-bot-smoke-fixture")


if __name__ == "__main__":
    unittest.main()

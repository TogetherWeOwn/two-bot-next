"""Duplicate-key refusal acceptance for scripts/staging_rollback_drill.py (offline only).

The ownership-control client parses the helper's stdout with a hookless
``json.loads`` (``ownership_control``), so a repeated object key currently
keeps the last value and the drill grades it: a repeated ``owner`` or
``phase`` can flip the active/fenced decision the drill and restore legs
act on, including firing the deployment POST.

Contract pinned here: any duplicate object key in that stdout — conflicting
in both orders, identical values, nested, or hidden behind an escaped
spelling — refuses with the existing fixed vocabulary
``ownership_control_invalid_response`` (no detail), before any deployment
POST, with a nonzero exit, a failures list naming exactly that refusal, and
no echo of the hostile keys or values. Duplicate-free stdout parses
byte-identically to ``json.loads``.

Stdlib only: the Cloudflare API is faked in memory, time is scripted, and
the ownership helper is a stubbed ``subprocess.run``. Nothing here moves
traffic or touches credentials.

This file is the contract: satisfy it by changing the drill script, not by
editing these tests.
"""

import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first,
# then imports the sibling module under test (no linter runs on scripts/).
import staging_rollback_drill as drill

GateError = drill.GateError

PRE = "11111111-1111-4111-8111-111111111111"
TARGET = "22222222-2222-4222-8222-222222222222"
OLDER = "33333333-3333-4333-8333-333333333333"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
DEPLOYMENTS_PATH = "/workers/scripts/two-bot-next-staging/deployments"

# Hostile markers are assembled at run time so the file stays scanner-clean.
HOSTILE_KEY = "hos" + "tile" + "-dup-key"
HOSTILE_VALUE = "hos" + "tile" + "-dup-value-9"

# Duplicate-key control stdouts. Pre-fix each parses last-wins and is graded;
# post-fix each refuses before any grading or POST.
CONFLICT_FENCED_WINS = (
    '{"owner": {"phase": "active", "epoch": 7}, "running": true, '
    '"owner": {"phase": "fenced", "epoch": 8}}'
)
CONFLICT_ACTIVE_WINS = (
    '{"owner": {"phase": "fenced", "epoch": 8}, "running": true, '
    '"owner": {"phase": "active", "epoch": 7}}'
)
IDENTICAL_REPEAT = (
    '{"owner": {"phase": "active", "epoch": 7}, "running": true, '
    '"owner": {"phase": "active", "epoch": 7}}'
)
NESTED_PHASE = (
    '{"owner": {"phase": "fenced", "phase": "active", "epoch": 7}, "running": true}'
)
ESCAPED_OWNER = (
    '{"owner": {"phase": "active", "epoch": 7}, "running": true, '
    '"\\u006fwner": {"phase": "fenced", "epoch": 8}}'
)
DUPLICATE_STDOUTS = {
    "conflict_fenced_wins": CONFLICT_FENCED_WINS,
    "conflict_active_wins": CONFLICT_ACTIVE_WINS,
    "identical_repeat": IDENTICAL_REPEAT,
    "nested_phase": NESTED_PHASE,
    "escaped_owner": ESCAPED_OWNER,
}
UNIQUE_STDOUT = (
    '{"owner": {"phase": "active", "epoch": 7}, "running": true, "deploymentId": "dep-1"}'
)
FENCE_OK = '{"owner": {"phase": "fenced", "epoch": 10}, "running": false}'


class World:
    """One staging singleton: Worker deployments plus a scripted clock."""

    def __init__(self):
        self.history = [PRE, TARGET, OLDER]  # newest first
        self.t = 1_800_000_000.0
        self.posts = []

    def tick(self, seconds):
        self.t += seconds


class FakeClient:
    """In-memory Cloudflare: deployments move on POST, probes always ready."""

    def __init__(self, world):
        self.world = world

    def api(self, path):
        if path == DEPLOYMENTS_PATH:
            return {"deployments": [
                {"id": "d", "versions": [{"version_id": version, "percentage": 100}]}
                for version in self.world.history]}
        # Container evidence is best effort; the drill continues without it.
        raise GateError("api_http_failure")

    def post(self, path, body):
        self.world.tick(1)
        self.world.posts.append((path, body))
        self.world.history.insert(0, body["versions"][0]["version_id"])
        return {"id": "new"}

    def request(self, url):
        version = self.world.history[0]
        if url.endswith("/health"):
            return 200, {}, b""
        body = json.dumps({"components": [["process", "ready"],
                                           ["gateway", "ready"]]}).encode()
        return 200, {"x-two-worker-version": version}, body


def scripted_control(status_stdout):
    """The real control client with a stubbed helper: status answers the
    canned stdout, every other action answers a valid fenced record."""
    control = drill.ownership_control("t" * 40, URL,
                                      "github-actions:700:rollback-drill", root="/repo")

    def fake_run(command, **kwargs):
        stdout = status_stdout if command[2] == "status" else FENCE_OK
        return drill.subprocess.CompletedProcess(args=command, returncode=0,
                                                 stdout=stdout, stderr="")

    patcher = mock.patch.object(drill.subprocess, "run", side_effect=fake_run)
    patcher.start()
    return control, patcher


def run_drill(status_stdout):
    world = World()
    control, patcher = scripted_control(status_stdout)
    logs = []
    instance = drill.Drill(FakeClient(world), control, URL, "700",
                           now=lambda: world.t, sleep=world.tick, log=logs.append)
    return instance, world, patcher, logs


class ControlParseTests(unittest.TestCase):
    def assert_refuses(self, stdout):
        control, patcher = scripted_control(stdout)
        self.addCleanup(patcher.stop)
        with self.assertRaises(GateError) as caught:
            control("status")
        self.assertEqual(str(caught.exception), "ownership_control_invalid_response")
        # The malformed-stdout path carries no detail; duplicates reuse it exactly.
        self.assertIsNone(caught.exception.detail)

    def test_conflicting_orders_refuse(self):
        for name in ("conflict_fenced_wins", "conflict_active_wins"):
            with self.subTest(name):
                self.assert_refuses(DUPLICATE_STDOUTS[name])

    def test_identical_values_refuse(self):
        self.assert_refuses(DUPLICATE_STDOUTS["identical_repeat"])

    def test_nested_duplicate_refuses(self):
        self.assert_refuses(DUPLICATE_STDOUTS["nested_phase"])

    def test_escaped_spelling_refuses(self):
        # The decoder resolves escapes before the hook runs, so this is a
        # repeat of "owner", not a new key. A text scan would miss it.
        self.assert_refuses(DUPLICATE_STDOUTS["escaped_owner"])


class RefusalBeforeActuationTests(unittest.TestCase):
    def test_duplicate_status_fails_the_drill_before_any_post(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                instance, world, patcher, _ = run_drill(stdout)
                self.addCleanup(patcher.stop)
                failures = instance.run(TARGET)
                # Exactly this refusal: nothing graded, no restore leg attempted.
                self.assertEqual(failures, ["drill:ownership_control_invalid_response"])
                self.assertEqual(world.posts, [])

    def test_unique_status_runs_the_drill_green(self):
        # Control: the world above is valid, so a duplicate-free status runs
        # the drill and its restore green, pre- and post-fix.
        instance, world, patcher, _ = run_drill(UNIQUE_STDOUT)
        self.addCleanup(patcher.stop)
        self.assertEqual(instance.run(TARGET), [])
        self.assertTrue(world.posts)


class MainTests(unittest.TestCase):
    def drive(self, status_stdout):
        world = World()
        control, patcher = scripted_control(status_stdout)
        self.addCleanup(patcher.stop)
        client = FakeClient(world)
        with tempfile.TemporaryDirectory() as tmp:
            evidence = str(Path(tmp) / "evidence.json")
            out = io.StringIO()
            env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": "t" * 40,
                   "GITHUB_RUN_ID": "700", "CLOUDFLARE_ACCOUNT_ID": "b" * 32,
                   "CLOUDFLARE_API_TOKEN": "x"}
            with mock.patch.object(drill, "ownership_control", return_value=control):
                with redirect_stdout(out):
                    code = drill.main(["--target-version", TARGET, "--evidence", evidence],
                                      environ=env, make_control=drill.ownership_control,
                                      client_factory=lambda account, token: client)
            receipt = json.loads(Path(evidence).read_text())
            return code, out.getvalue(), receipt, list(world.posts)

    def test_duplicate_status_fails_closed_with_fixed_failures(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                code, out, receipt, posts = self.drive(stdout)
                self.assertEqual(code, 1, out)
                self.assertEqual(receipt["failures"],
                                 ["drill:ownership_control_invalid_response"])
                self.assertEqual(posts, [])

    def test_hostile_keys_and_values_never_echo(self):
        stdout = ('{"owner": {"phase": "active", "epoch": 7}, "running": true, '
                  f'"{HOSTILE_KEY}": "{HOSTILE_VALUE}", "{HOSTILE_KEY}": "other"}}')
        control, patcher = scripted_control(stdout)
        self.addCleanup(patcher.stop)
        with self.assertRaises(GateError):
            control("status")
        code, out, receipt, _ = self.drive(stdout)
        self.assertEqual(code, 1, out)
        self.assertNotIn(HOSTILE_KEY, out)
        self.assertNotIn(HOSTILE_VALUE, out)
        self.assertNotIn(HOSTILE_KEY, json.dumps(receipt))
        self.assertNotIn(HOSTILE_VALUE, json.dumps(receipt))

    def test_unique_stdout_parses_byte_identically(self):
        control, patcher = scripted_control(UNIQUE_STDOUT)
        self.addCleanup(patcher.stop)
        self.assertEqual(control("status"), json.loads(UNIQUE_STDOUT))


if __name__ == "__main__":
    unittest.main()

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

Finite windows: ``main`` always builds the default ``StagingDrillUse``, so the
fixture supplies an explicit run attempt and use receipt, finite-policy
immutable version metadata and exact-SHA readiness. The stubbed
``subprocess.run`` routes by helper script: only ``ownership-control.mjs``
answers the canned stdout, and the lifecycle helpers get their own recording
stand-in. Each duplicate case is therefore proven to reach the ownership
response decoder (not a missing-lifecycle refusal), before any lifecycle
acquisition or after it with a confirmed release, never with a POST.

Stdlib only: the Cloudflare API is faked in memory, time is scripted, and
the ownership and lifecycle helpers are a stubbed ``subprocess.run``. Nothing
here moves traffic, runs node or touches credentials.

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
VERSIONS_PATH = "/workers/scripts/two-bot-next-staging/versions/"
SHA = "a" * 40
LIFECYCLE_HELPERS = ("staging-use.mjs", "staging-use-pipeline.mjs")

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
        if path.startswith(VERSIONS_PATH):
            # Immutable finite-policy metadata: same build on every version.
            version = path[len(VERSIONS_PATH):]
            return {"id": version, "annotations": {"workers/message": SHA},
                    "resources": {"bindings": [
                        {"name": "STAGING_RUNTIME_POLICY", "type": "plain_text", "text": "bounded"},
                        {"name": "TWO_DEPLOY_ENV", "type": "plain_text", "text": "staging"},
                        {"name": "CONTAINER_RELEASE", "type": "plain_text", "text": SHA}]}}
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
                                           ["gateway", "ready"]],
                           "build_revision": SHA}).encode()
        return 200, {"x-two-worker-version": version}, body


def scripted_control(status_stdout, statuses=None):
    """The real control client with a stubbed helper: status answers the
    canned stdout (or the nth entry of ``statuses``), every other ownership
    action answers a valid fenced record. Lifecycle helpers never see the
    canned stdout: they are recorded on ``patcher.lifecycle`` and succeed.
    Any other command is recorded on ``patcher.unexpected`` and refused."""
    control = drill.ownership_control("t" * 40, URL,
                                      "github-actions:700:rollback-drill", root="/repo")

    def fake_run(command, **kwargs):
        name = Path(str(command[1])).name
        if name in LIFECYCLE_HELPERS:
            patcher.lifecycle.append(command[2])
            return drill.subprocess.CompletedProcess(args=command, returncode=0)
        if name != "ownership-control.mjs":
            patcher.unexpected.append(name)
            raise OSError("unexpected subprocess")
        patcher.ownership.append(command[2])
        stdout = FENCE_OK
        if command[2] == "status":
            count = patcher.ownership.count("status")
            stdout = (statuses[count - 1] if statuses and count <= len(statuses)
                      else status_stdout)
        return drill.subprocess.CompletedProcess(args=command, returncode=0,
                                                 stdout=stdout, stderr="")

    patcher = mock.patch.object(drill.subprocess, "run", side_effect=fake_run)
    patcher.lifecycle, patcher.ownership, patcher.unexpected = [], [], []
    patcher.start()
    return control, patcher


def finite_use(world):
    """The real default lifecycle helper on the scripted clock."""
    return drill.StagingDrillUse(
        {"GITHUB_RUN_ID": "700", "GITHUB_RUN_ATTEMPT": "1",
         "STAGING_USE_RECEIPT": "/offline/use.ndjson"},
        URL, "t" * 40, now=lambda: world.t, root="/repo")


def run_drill(status_stdout):
    world = World()
    control, patcher = scripted_control(status_stdout)
    logs = []
    instance = drill.Drill(FakeClient(world), control, URL, "700",
                           now=lambda: world.t, sleep=world.tick, log=logs.append)
    return instance, world, patcher, logs


def _refuse_real_subprocess(command, **_kwargs):
    raise AssertionError("offline acceptance: unpatched subprocess " + Path(str(command[1])).name)


_GUARD = mock.patch.object(drill.subprocess, "run", _refuse_real_subprocess)


def setUpModule():
    """No test may reach real node: scripted_control patches its own fake."""
    _GUARD.start()


def tearDownModule():
    _GUARD.stop()


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

    def test_duplicate_status_refuses_before_lifecycle_acquisition(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                world = World()
                control, patcher = scripted_control(stdout)
                self.addCleanup(patcher.stop)
                use = finite_use(world)
                instance = drill.Drill(FakeClient(world), control, URL, "700",
                                       now=lambda: world.t, sleep=world.tick,
                                       log=lambda _line: None, staging_use=use)
                failures = instance.run(TARGET)
                self.assertEqual(failures, ["drill:ownership_control_invalid_response"])
                # The decoder was reached on the first status, not skipped by
                # a missing-window refusal; nothing was acquired or posted.
                self.assertEqual(patcher.ownership, ["status"])
                self.assertEqual(patcher.lifecycle, [])
                self.assertEqual(patcher.unexpected, [])
                self.assertEqual(world.posts, [])
                self.assertEqual(instance.evidence["staging_cleanup"], "not_acquired")

    def test_duplicate_status_after_acquire_releases_without_post(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                world = World()
                control, patcher = scripted_control(UNIQUE_STDOUT, statuses=[UNIQUE_STDOUT, stdout])
                self.addCleanup(patcher.stop)
                instance = drill.Drill(FakeClient(world), control, URL, "700",
                                       now=lambda: world.t, sleep=world.tick,
                                       log=lambda _line: None, staging_use=finite_use(world))
                failures = instance.run(TARGET)
                self.assertEqual(failures, ["drill:ownership_control_invalid_response"])
                self.assertEqual(patcher.ownership, ["status", "status"])
                self.assertEqual(patcher.lifecycle, ["acquire", "ready", "release"])
                self.assertEqual(world.posts, [])
                self.assertEqual(instance.evidence["staging_cleanup"], "release_confirmed")

    def test_duplicate_restore_status_never_reaches_restore_post(self):
        # Status order: preflight, post-acquire, drill leg, restore leg.
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                world = World()
                control, patcher = scripted_control(
                    UNIQUE_STDOUT, statuses=[UNIQUE_STDOUT] * 3 + [stdout])
                self.addCleanup(patcher.stop)
                instance = drill.Drill(FakeClient(world), control, URL, "700",
                                       now=lambda: world.t, sleep=world.tick,
                                       log=lambda _line: None, staging_use=finite_use(world))
                failures = instance.run(TARGET)
                self.assertEqual(failures, ["restore:ownership_control_invalid_response"])
                self.assertEqual(len(world.posts), 1, "only the drill leg posted")
                self.assertEqual(patcher.ownership[-1], "status")
                self.assertNotIn("restore-check", patcher.lifecycle[3:],
                                 "a restore never asserts the active owner")

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
                   "GITHUB_RUN_ID": "700", "GITHUB_RUN_ATTEMPT": "1",
                   "STAGING_USE_RECEIPT": str(Path(tmp) / "use.ndjson"),
                   "CLOUDFLARE_ACCOUNT_ID": "b" * 32, "CLOUDFLARE_API_TOKEN": "x"}
            with mock.patch.object(drill, "ownership_control", return_value=control):
                with redirect_stdout(out):
                    code = drill.main(["--target-version", TARGET, "--evidence", evidence],
                                      environ=env, make_control=drill.ownership_control,
                                      client_factory=lambda account, token: client)
            receipt = json.loads(Path(evidence).read_text())
            self.patcher = patcher
            return code, out.getvalue(), receipt, list(world.posts)

    def test_duplicate_status_fails_closed_with_fixed_failures(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                code, out, receipt, posts = self.drive(stdout)
                self.assertEqual(code, 1, out)
                self.assertEqual(receipt["failures"],
                                 ["drill:ownership_control_invalid_response"])
                self.assertEqual(posts, [])
                # main's default lifecycle helper was built and the refusal
                # came from the ownership decoder, before any acquisition.
                self.assertEqual(self.patcher.ownership, ["status"])
                self.assertEqual(self.patcher.lifecycle, [])
                self.assertEqual(self.patcher.unexpected, [])
                self.assertEqual(receipt["staging_cleanup"], "not_acquired")

    def test_unique_status_main_runs_green_and_releases(self):
        # Control: main's default finite-use lifecycle runs end to end.
        with mock.patch.object(drill, "READY_TIMEOUT_SECONDS", 5):
            code, out, receipt, posts = self.drive(UNIQUE_STDOUT)
        self.assertEqual(code, 0, out)
        self.assertEqual(receipt["failures"], [])
        self.assertTrue(posts)
        self.assertEqual(self.patcher.lifecycle,
                         ["acquire", "ready", "restore-check", "acquire", "acquire", "release"])
        self.assertEqual(receipt["staging_cleanup"], "release_confirmed")

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
        self.assertEqual(self.patcher.ownership, ["status"], "the decoder must be reached")
        self.assertEqual(self.patcher.lifecycle, [])

    def test_unique_stdout_parses_byte_identically(self):
        control, patcher = scripted_control(UNIQUE_STDOUT)
        self.addCleanup(patcher.stop)
        self.assertEqual(control("status"), json.loads(UNIQUE_STDOUT))


if __name__ == "__main__":
    unittest.main()

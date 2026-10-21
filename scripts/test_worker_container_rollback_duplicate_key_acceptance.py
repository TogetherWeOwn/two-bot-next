"""Duplicate-key refusal acceptance for scripts/worker_container_rollback.py (offline only).

The ownership-control client parses the helper's stdout with a hookless
``json.loads`` (``control_client``), so a repeated object key currently keeps
the last value and the rollback grades it: a repeated ``owner`` or ``phase``
can flip the fenced/active decision the restore acts on.

Contract pinned here: any duplicate object key in that stdout — conflicting
in both orders, identical values, nested, or hidden behind an escaped
spelling — refuses with the existing fixed vocabulary
``ownership_control_invalid_response`` (no detail), before any restore POST,
with a nonzero exit, no evidence file, and no echo of the hostile keys or
values. Duplicate-free stdout parses byte-identically to ``json.loads``.

Stdlib only: the Cloudflare API is faked in memory, time is scripted, and
the ownership helper is a stubbed ``subprocess.run``. Nothing here moves
traffic or touches credentials.

This file is the contract: satisfy it by changing the rollback script, not
by editing these tests.
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
import staging_rollout as base
import worker_container_rollback as rollback

ACCOUNT = "a" * 32
WORKER = "two-bot-next-staging"
APPLICATION = "two-bot-next-twobotcontainer-staging"
APP_ID = "app-1"
NAMESPACE = "ns-1"
GOOD_VERSION = "11111111-1111-4111-8111-111111111111"
BAD_VERSION = "22222222-2222-4222-8222-222222222222"
SHA = "0123456789abcdef0123456789abcdef01234567"
IMAGE_AT = "registry.cloudflare.com/acct/two-bot-next-twobotcontainer-staging@sha256:"
WORKER_URL = "https://two-bot-next-staging.acceptance-fixture.workers.dev"
STAGING_URL = "https://two-bot-next-staging.acceptance-fixture.workers.dev"

# Hostile markers are assembled at run time so the file stays scanner-clean.
HOSTILE_KEY = "hos" + "tile" + "-dup-key"
HOSTILE_VALUE = "hos" + "tile" + "-dup-value-9"


def good_image():
    return IMAGE_AT + "a" * 64


# Duplicate-key control stdouts. Pre-fix each parses last-wins and the run
# below completes; post-fix each refuses before any grading or POST.
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


def rollout_row(rollout_id, image, created="2026-10-15T00:00:00Z", status="completed",
                target_version=7):
    return {"id": rollout_id, "created_at": created, "status": status,
            "strategy": "rolling", "kind": "full_auto", "target_version": target_version,
            "current_version": target_version - 1,
            "target_configuration": {"image": image, "instance_type": "lite"},
            "current_configuration": {"image": image},
            "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                     "starting": 0, "scheduling": 0}},
            "progress": {"total_steps": 1, "current_step": 1, "total_instances": 1,
                         "updated_instances": 1},
            "steps": [{"status": "completed"}]}


def ready_probe(version, revision=SHA, gateway="ready", health=200, status=200):
    def handler():
        body = json.dumps({"components": [["process", "ready"], ["gateway", gateway],
                                           ["database", "ready"], ["token_invalid", "ready"]],
                           "jobs": {}, "build_revision": revision,
                           "build_id": "1-1"}).encode()
        return status, {"x-two-worker-version": version}, body

    def health_handler():
        return health, {}, b"{}"

    return handler, health_handler


class FakeClient(rollback.RollbackClient):
    """In-memory Cloudflare: routes are (method, path) -> handler."""

    def __init__(self, routes):
        super().__init__(ACCOUNT, "token")
        self.routes = routes
        self.calls = []

    def api(self, path):
        self.calls.append(("GET", path))
        data = self.routes[("GET", path)]()
        base.require(base.mapping(data).get("success") is True and "result" in data,
                     "api_failure")
        return data["result"]

    def post(self, path, body):
        self.calls.append(("POST", path, body))
        return self.routes[("POST", path)](body)

    def request(self, url, authenticated=False):
        self.calls.append(("PROBE", url))
        return self.routes[("PROBE", url.rsplit("/", 1)[-1])]()


class Clock:
    def __init__(self):
        self.t = 0.0

    def now(self):
        return self.t

    def sleep(self, seconds):
        self.t += seconds


def scripted_control(status_stdout):
    """The real control client with a stubbed helper: status answers the
    canned stdout, every other action answers a valid fenced record."""
    control = rollback.control_client("staging", "t" * 32, WORKER_URL, STAGING_URL,
                                      "test-actor")

    def fake_run(command, **kwargs):
        stdout = status_stdout if command[2] == "status" else FENCE_OK
        return rollback.subprocess.CompletedProcess(args=command, returncode=0,
                                                    stdout=stdout, stderr="")

    patcher = mock.patch.object(rollback.subprocess, "run", side_effect=fake_run)
    patcher.start()
    return control, patcher


def world_routes():
    """A world where BAD serves and GOOD is a validated manual target: the
    fence-first status read precedes every restore POST."""
    prev = rollout_row("r-prev", good_image(), created="2026-10-14T00:00:00Z")
    created = {"n": 0}
    state = {"serving": [BAD_VERSION, GOOD_VERSION]}

    def post_rollouts(body):
        created["n"] += 1
        return {"id": "r-new", "target_version": 9}

    def get_rollout():
        status = "completed" if created["n"] else "progressing"
        return {"success": True, "result": rollout_row(
            "r-new", good_image(), created="2026-10-15T01:00:00Z",
            status=status, target_version=9)}

    def post_deployments(body):
        state["serving"][0] = body["versions"][0]["version_id"]
        return {"ok": True}

    handler, health = ready_probe(GOOD_VERSION)
    routes = {
        ("GET", f"/workers/scripts/{WORKER}/deployments"):
            lambda: {"success": True, "result": {"deployments": [
                {"versions": [{"version_id": version, "percentage": 100}]}
                for version in state["serving"]]}},
        ("POST", f"/workers/scripts/{WORKER}/deployments"): post_deployments,
        ("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"):
            lambda: {"success": True, "result": {
                "id": GOOD_VERSION, "annotations": {"workers/message": SHA}}},
        ("GET", "/containers/applications"): lambda: {
            "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                         "durable_objects": {"namespace_id": NAMESPACE},
                                         "configuration": {"image": good_image()}}]},
        ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"):
            lambda: {"success": True, "result": [prev]},
        ("POST", f"/containers/applications/{APP_ID}/rollouts"): post_rollouts,
        ("GET", f"/containers/applications/{APP_ID}/rollouts/r-new"): get_rollout,
        ("PROBE", "readyz"): handler,
        ("PROBE", "health"): health,
    }
    return routes


def posts_of(client):
    return [call for call in client.calls if call[0] == "POST"]


class ControlParseTests(unittest.TestCase):
    def assert_refuses(self, stdout):
        control, patcher = scripted_control(stdout)
        self.addCleanup(patcher.stop)
        with self.assertRaises(base.GateError) as caught:
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
    def test_duplicate_status_refuses_before_any_restore_post(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                control, patcher = scripted_control(stdout)
                self.addCleanup(patcher.stop)
                log = io.StringIO()
                client = FakeClient(world_routes())
                clock = Clock()
                instance = rollback.Rollback(
                    client, control, WORKER_URL, "staging", WORKER, APPLICATION,
                    APP_ID, SHA, "700", now=clock.now, sleep=clock.sleep,
                    log=lambda msg: log.write(msg + "\n"))
                with self.assertRaises(base.GateError) as caught:
                    instance.run(GOOD_VERSION, good_image())
                self.assertEqual(str(caught.exception),
                                 "ownership_control_invalid_response")
                self.assertEqual(posts_of(client), [])

    def test_unique_status_completes_the_restore(self):
        # Control: the world above is valid, so a duplicate-free status runs
        # the whole restore green, pre- and post-fix.
        control, patcher = scripted_control(UNIQUE_STDOUT)
        self.addCleanup(patcher.stop)
        log = io.StringIO()
        client = FakeClient(world_routes())
        clock = Clock()
        instance = rollback.Rollback(
            client, control, WORKER_URL, "staging", WORKER, APPLICATION,
            APP_ID, SHA, "700", now=clock.now, sleep=clock.sleep,
            log=lambda msg: log.write(msg + "\n"))
        evidence = instance.run(GOOD_VERSION, good_image())
        self.assertEqual(evidence["restored_version"], GOOD_VERSION)
        self.assertTrue(posts_of(client))


class MainTests(unittest.TestCase):
    def drive(self, status_stdout):
        control, patcher = scripted_control(status_stdout)
        self.addCleanup(patcher.stop)
        client = FakeClient(world_routes())
        with tempfile.TemporaryDirectory() as tmp:
            evidence = str(Path(tmp) / "evidence.json")
            argv = ["rollback", "--variant", "staging", "--worker-url", WORKER_URL,
                   "--expected-sha", SHA, "--target-version", GOOD_VERSION,
                   "--target-image", good_image(), "--actor", "test-actor",
                   "--evidence", evidence]
            env = {"OWNERSHIP_CONTROL_TOKEN": "t" * 40, "GITHUB_RUN_ID": "700"}
            out = io.StringIO()
            with mock.patch.object(rollback, "control_client", return_value=control):
                # Timing is not pinned here: main() builds its own clock, so
                # collapse the poll interval and keep the pre-fix full run instant.
                with mock.patch.object(rollback, "POLL_SECONDS", 0):
                    with redirect_stdout(out):
                        code = rollback.main(argv, environ=env,
                                             client_factory=lambda account, token: client)
            return code, out.getvalue(), Path(evidence), posts_of(client)

    def test_duplicate_status_fails_closed_with_no_evidence_or_posts(self):
        for name, stdout in DUPLICATE_STDOUTS.items():
            with self.subTest(name):
                code, out, evidence, posts = self.drive(stdout)
                self.assertEqual(code, 1, out)
                self.assertIn("worker-container rollback failed: "
                              "ownership_control_invalid_response", out)
                self.assertNotIn("diagnostic", out)
                self.assertFalse(evidence.exists(), out)
                self.assertEqual(posts, [])

    def test_hostile_keys_and_values_never_echo(self):
        stdout = ('{"owner": {"phase": "active", "epoch": 7}, '
                  f'"{HOSTILE_KEY}": "{HOSTILE_VALUE}", "{HOSTILE_KEY}": "other"}}')
        control, patcher = scripted_control(stdout)
        self.addCleanup(patcher.stop)
        with self.assertRaises(base.GateError):
            control("status")
        code, out, evidence, _ = self.drive(stdout)
        self.assertEqual(code, 1, out)
        self.assertNotIn(HOSTILE_KEY, out)
        self.assertNotIn(HOSTILE_VALUE, out)
        self.assertFalse(evidence.exists(), out)

    def test_unique_stdout_parses_byte_identically(self):
        control, patcher = scripted_control(UNIQUE_STDOUT)
        self.addCleanup(patcher.stop)
        self.assertEqual(control("status"), json.loads(UNIQUE_STDOUT))


if __name__ == "__main__":
    unittest.main()

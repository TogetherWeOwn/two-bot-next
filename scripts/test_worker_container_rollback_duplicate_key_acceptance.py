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

Staging restores also run inside the caller's finite-use window: the
target version carries the bounded policy bindings, and the rollback's
``restore-check`` precedes the fence-first status read. The duplicate
refusal therefore happens after a passing window check and before any
mutation, and never reaches ``rebind``. The finite-use pipeline is answered
by its own acceptance stand-in, never by the ownership stdout stub, so a
duplicate-key status cannot be graded as (or masked by) a window answer.
Missing, other-attempt and expired windows refuse before any ownership
call or POST.

Stdlib only: the Cloudflare API is faked in memory, time is scripted, and
the ownership helper and finite-use pipeline are a stubbed
``subprocess.run`` dispatched by script name. Nothing here moves traffic or
touches credentials.

This file is the contract: satisfy it by changing the rollback script, not
by editing these tests.
"""

import io
import json
import shutil
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
OWNERSHIP_SCRIPT = "ownership-control.mjs"
FINITE_USE_SCRIPT = "staging-use-pipeline.mjs"
# The deploy's original finite-use window; rollback must keep it, not mint one.
RUN_ID, RUN_ATTEMPT, EXPIRES_AT = "700", "2", "1700001000000"
TOKEN = "t" * 40
CF_CANARY = "dup" + "key" + "-cf-canary-1234567890abcdef"

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


def window_env(**overrides):
    env = {"GITHUB_RUN_ID": RUN_ID, "GITHUB_RUN_ATTEMPT": RUN_ATTEMPT,
           "STAGING_USE_EXPIRES_AT": EXPIRES_AT, "PATH": "/usr/bin"}
    env.update(overrides)
    return {key: value for key, value in env.items() if value is not None}


class FiniteUseWindow:
    """Acceptance-only stand-in for wrangler/scripts/staging-use-pipeline.mjs.

    It holds one granted (run, attempt, expiry) and answers restore-check and
    rebind with exit 0 only inside that grant; anything else (another
    attempt, a later expiry, an expired grant, any other action such as a
    fresh `begin`) exits 1. `trace` interleaves its actions with ownership
    actions; `client` (when set) is snapshotted at each action.
    """

    def __init__(self, attempt=RUN_ATTEMPT, expired=False, refuse=()):
        self.grant = (RUN_ID, attempt, EXPIRES_AT)
        self.expired, self.refuse = expired, set(refuse)
        self.calls, self.trace, self.client = [], [], None

    def __call__(self, command, **kwargs):
        env = kwargs.get("env") or {}
        action = command[2]
        window = tuple(env.get(key) for key in
                       ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "STAGING_USE_EXPIRES_AT"))
        self.calls.append({"action": action, "window": window, "env": dict(env),
                           "stdout": kwargs.get("stdout"), "stderr": kwargs.get("stderr"),
                           "client_calls": list(self.client.calls) if self.client else []})
        self.trace.append(("use", action))
        ok = (action in ("restore-check", "rebind") and window == self.grant
              and not self.expired and action not in self.refuse)
        return rollback.subprocess.CompletedProcess(args=command, returncode=0 if ok else 1)

    @property
    def actions(self):
        return [call["action"] for call in self.calls]


def scripted_control(status_stdout, window=None):
    """The real control client with a stubbed helper: status answers the
    canned stdout, every other action answers a valid fenced record. The
    finite-use pipeline goes to `window`, never to this canned stdout; any
    other subprocess fails the test."""
    control = rollback.control_client("staging", "t" * 32, WORKER_URL, STAGING_URL,
                                      "test-actor")
    window = FiniteUseWindow() if window is None else window

    def fake_run(command, **kwargs):
        script = Path(str(command[1])).name
        if script == FINITE_USE_SCRIPT:
            return window(command, **kwargs)
        if script != OWNERSHIP_SCRIPT:
            raise AssertionError(f"unexpected subprocess: {script}")
        window.trace.append(("control", command[2]))
        stdout = status_stdout if command[2] == "status" else FENCE_OK
        return rollback.subprocess.CompletedProcess(args=command, returncode=0,
                                                    stdout=stdout, stderr="")

    patcher = mock.patch.object(rollback.subprocess, "run", side_effect=fake_run)
    patcher.start()
    return control, patcher


def staging_use(window, receipt="evidence.json.staging-use.jsonl"):
    """The real finite-use adapter for direct Rollback runs; its subprocess
    is answered by `window` through scripted_control's dispatcher."""
    return rollback.staging_restore_client(window_env(), WORKER_URL, "t" * 32,
                                           GOOD_VERSION, SHA, receipt)


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
                "id": GOOD_VERSION, "annotations": {"workers/message": SHA},
                "resources": {"bindings": [
                    {"name": "STAGING_RUNTIME_POLICY", "type": "plain_text",
                     "text": "bounded"},
                    {"name": "TWO_DEPLOY_ENV", "type": "plain_text", "text": "staging"},
                    {"name": "CONTAINER_RELEASE", "type": "plain_text", "text": SHA}]}}},
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
                window = FiniteUseWindow()
                control, patcher = scripted_control(stdout, window)
                self.addCleanup(patcher.stop)
                log = io.StringIO()
                client = FakeClient(world_routes())
                clock = Clock()
                instance = rollback.Rollback(
                    client, control, WORKER_URL, "staging", WORKER, APPLICATION,
                    APP_ID, SHA, "700", now=clock.now, sleep=clock.sleep,
                    log=lambda msg: log.write(msg + "\n"), staging_use=staging_use(window))
                with self.assertRaises(base.GateError) as caught:
                    instance.run(GOOD_VERSION, good_image())
                self.assertEqual(str(caught.exception),
                                 "ownership_control_invalid_response")
                self.assertEqual(posts_of(client), [])
                # The window check passed first; the duplicate, not the
                # window, is what refused, and nothing rebinds after it.
                self.assertEqual(window.actions, ["restore-check"])
                self.assertEqual(window.trace[0], ("use", "restore-check"))
                self.assertNotIn(("control", "fence"), window.trace)

    def test_unique_status_completes_the_restore(self):
        # Control: the world above is valid, so a duplicate-free status runs
        # the whole restore green, pre- and post-fix.
        window = FiniteUseWindow()
        control, patcher = scripted_control(UNIQUE_STDOUT, window)
        self.addCleanup(patcher.stop)
        log = io.StringIO()
        client = FakeClient(world_routes())
        window.client = client
        clock = Clock()
        instance = rollback.Rollback(
            client, control, WORKER_URL, "staging", WORKER, APPLICATION,
            APP_ID, SHA, "700", now=clock.now, sleep=clock.sleep,
            log=lambda msg: log.write(msg + "\n"), staging_use=staging_use(window))
        evidence = instance.run(GOOD_VERSION, good_image())
        self.assertEqual(evidence["restored_version"], GOOD_VERSION)
        self.assertTrue(posts_of(client))
        self.assertEqual(evidence["staging_use"], "retained-window-rebound-ready")
        check, rebind = window.calls
        self.assertEqual((check["action"], rebind["action"]), ("restore-check", "rebind"))
        # Same run/attempt/expiry for both; restore-check precedes every
        # ownership action and POST; rebind follows takeover, precedes probes.
        self.assertEqual(check["window"], (RUN_ID, RUN_ATTEMPT, EXPIRES_AT))
        self.assertEqual(rebind["window"], check["window"])
        self.assertEqual(window.trace[0], ("use", "restore-check"))
        self.assertFalse([c for c in check["client_calls"] if c[0] in ("POST", "PROBE")])
        rebind_at = window.trace.index(("use", "rebind"))
        self.assertTrue(any(entry[0] == "control" and "takeover" in entry[1]
                            for entry in window.trace[:rebind_at]), window.trace)
        self.assertTrue([c for c in rebind["client_calls"] if c[0] == "POST"])
        self.assertFalse([c for c in rebind["client_calls"] if c[0] == "PROBE"])
        self.assertTrue([c for c in client.calls if c[0] == "PROBE"])


class MainTests(unittest.TestCase):
    def drive(self, status_stdout, window=None, env=None):
        self.window = FiniteUseWindow() if window is None else window
        control, patcher = scripted_control(status_stdout, self.window)
        self.addCleanup(patcher.stop)
        client = FakeClient(world_routes())
        self.window.client = client
        # A per-call directory that outlives drive(), so the evidence
        # assertions below observe the run instead of a deleted tempdir.
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp, True)
        evidence = str(Path(tmp) / "evidence.json")
        argv = ["rollback", "--variant", "staging", "--worker-url", WORKER_URL,
               "--expected-sha", SHA, "--target-version", GOOD_VERSION,
               "--target-image", good_image(), "--actor", "test-actor",
               "--evidence", evidence]
        env = dict(window_env(), OWNERSHIP_CONTROL_TOKEN=TOKEN,
                   CLOUDFLARE_API_TOKEN=CF_CANARY) if env is None else env
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
                self.assertEqual(self.window.actions, ["restore-check"])
                self.assertNotIn(("control", "fence"), self.window.trace)

    def test_unique_status_main_restores_inside_the_retained_window(self):
        # GREEN control: the only difference from the duplicate runs is the
        # status stdout, so they fail on the duplicate, not on harness.
        code, out, evidence, posts = self.drive(UNIQUE_STDOUT)
        self.assertEqual(code, 0, out)
        self.assertTrue(posts)
        written = json.loads(evidence.read_text())
        self.assertEqual(written["staging_use"], "retained-window-rebound-ready")
        self.assertEqual(written["restored_version"], GOOD_VERSION)
        self.assertEqual(self.window.actions, ["restore-check", "rebind"])
        for call in self.window.calls:
            self.assertEqual(call["window"], (RUN_ID, RUN_ATTEMPT, EXPIRES_AT))
            self.assertEqual(call["env"]["STAGING_USE_DEPLOYMENT"], GOOD_VERSION)
            self.assertEqual(call["env"]["STAGING_USE_SHA"], SHA)
            self.assertEqual(call["env"]["STAGING_USE_RECEIPT"],
                             str(evidence) + ".staging-use.jsonl")
            # Redacting adapter: no ambient Cloudflare credential, no
            # captured child output that could reach the log.
            self.assertNotIn("CLOUDFLARE_API_TOKEN", call["env"])
            self.assertEqual(call["stdout"], rollback.subprocess.DEVNULL)
            self.assertEqual(call["stderr"], rollback.subprocess.DEVNULL)
        self.assertNotIn(TOKEN, out)
        self.assertNotIn(CF_CANARY, out)

    def test_missing_window_refuses_before_any_control_or_post(self):
        for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "STAGING_USE_EXPIRES_AT"):
            with self.subTest(missing=key):
                env = dict(window_env(**{key: None}), OWNERSHIP_CONTROL_TOKEN=TOKEN)
                code, out, evidence, posts = self.drive(UNIQUE_STDOUT, env=env)
                self.assertEqual(code, 1, out)
                self.assertIn("worker-container rollback failed: "
                              "staging_restore_window_missing", out)
                self.assertEqual(posts, [])
                self.assertEqual(self.window.trace, [])
                self.assertFalse(evidence.exists(), out)

    def test_other_attempt_or_expired_window_refuses_before_mutation(self):
        worlds = {"other-attempt": FiniteUseWindow(attempt="1"),
                  "expired": FiniteUseWindow(expired=True)}
        for name, window in worlds.items():
            with self.subTest(name):
                code, out, evidence, posts = self.drive(UNIQUE_STDOUT, window=window)
                self.assertEqual(code, 1, out)
                self.assertIn("worker-container rollback failed: "
                              "staging_restore_control_failed", out)
                self.assertNotIn("diagnostic", out)
                self.assertEqual(posts, [])
                # No fresh recovery window, no ownership call of any kind.
                self.assertEqual(window.trace, [("use", "restore-check")])
                self.assertFalse(evidence.exists(), out)

    def test_refused_rebind_never_probes_or_writes_evidence(self):
        window = FiniteUseWindow(refuse={"rebind"})
        code, out, evidence, posts = self.drive(UNIQUE_STDOUT, window=window)
        self.assertEqual(code, 1, out)
        self.assertIn("worker-container rollback failed: staging_restore_control_failed", out)
        self.assertEqual(window.actions, ["restore-check", "rebind"])
        self.assertTrue(posts)
        self.assertFalse([call for call in window.client.calls if call[0] == "PROBE"])
        self.assertFalse(evidence.exists(), out)

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

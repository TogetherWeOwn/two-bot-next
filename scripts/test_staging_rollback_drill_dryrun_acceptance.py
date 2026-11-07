#!/usr/bin/env python3
"""Acceptance tests: staging rollback drill --dry-run previews the plan.

DR (staging drill dry-run). Offline only: stubbed Cloudflare client and
ownership control, a recording ``subprocess.run`` stand-in for the finite-use
lifecycle helpers, no network, no real node, no credentials, no databases.

Contract under test
-------------------
1. ``staging_rollback_drill.py --target-version <uuid> --dry-run`` with a
   valid environment exits 0, prints a stable plan to stdout, and performs
   zero mutations. Stable means byte-identical stdout across two runs with
   the same inputs (no wall-clock in the plan). The plan names ``dry-run``
   and ``plan``, the target version, the serving (pre) version, and both
   legs ``drill`` then ``restore`` in order.
2. Zero mutations means the stubbed ``client.post`` is never called and the
   control is never called with ``fence`` or ``deployment-takeover``
   (``status`` reads are allowed). History, phase and epoch are unchanged.
   Dry-run also never invokes a finite-use lifecycle helper (no acquire,
   ready, restore-check or release) and never writes the use receipt.
3. Plan/live pre-mutation parity: every invalid input class refuses with the
   identical code in ``--dry-run`` and live mode, before any mutation and
   before any lifecycle acquisition. The evidence file shape for
   ``--dry-run`` is unspecified and not asserted.
4. RED is behavioral: every failing assertion below fails on an exit code,
   plan-text or call-count comparison against live code paths, never on an
   import error, missing fixture or refusal to run.
5. Finite windows (on-demand staging, no always-on target). ``main`` always
   builds the default ``StagingDrillUse`` and requires an explicit
   ``GITHUB_RUN_ATTEMPT`` and ``STAGING_USE_RECEIPT``. Both the serving and
   the target Worker versions must carry a ``workers/message`` full SHA and
   exactly one ``plain_text`` binding each for
   ``STAGING_RUNTIME_POLICY=bounded``, ``TWO_DEPLOY_ENV=staging`` and
   ``CONTAINER_RELEASE=<that SHA>``, and both SHAs must match (a Worker-only
   drill never changes the container image). Unknown, always-on, pre-stamp
   or mismatched targets refuse before any mutation in both modes. A
   running baseline that is not exact-SHA ready refuses before acquisition.
   An idle baseline may be started by live mode (acquire + ready, then
   exact-SHA readiness) but dry-run refuses it instead of claiming a live
   startup. Live begins exactly one 25-minute run/attempt grant, asserts it
   before the first fence, and after each takeover reacquires the same
   persisted grant at the new epoch before the readiness loop. A fenced
   restore never asserts the active owner, stays inside the original
   deadline, and must reacquire before the runtime can start. An
   authentication failure or a lifecycle refusal stops every later control,
   restore and release (native expiry owns teardown); an ordinary ready
   timeout still restores. Cleanup must confirm the stop or fail nonzero
   without claiming idle.

Controls assert the existing live refusal vocabulary is unchanged; they
pass before and after the builder's change.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_rollback_drill_dryrun_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import json
import subprocess
import time
import unittest
import tempfile
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "staging_rollback_drill_dryrun_under_test",
    Path(__file__).with_name("staging_rollback_drill.py"))
drill = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drill)
GateError = drill.GateError
StagingDrillUse = drill.StagingDrillUse

PRE = "11111111-1111-4111-8111-111111111111"
TARGET = "22222222-2222-4222-8222-222222222222"
OLDER = "33333333-3333-4333-8333-333333333333"
NEVER = "44444444-4444-4444-8444-444444444444"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
ACCOUNT = "b" * 32
TOKEN = "t" * 40
CF_TOKEN = "c" * 40
SHA = "a" * 40
OTHER_SHA = "d" * 40
DEPLOYMENTS_PATH = "/workers/scripts/two-bot-next-staging/deployments"
VERSIONS_PATH = "/workers/scripts/two-bot-next-staging/versions/"
WINDOW_MS = 25 * 60 * 1000
RECEIPT = object()  # invoke() swaps this for a path inside its temp directory
LIFECYCLE_HELPERS = ("staging-use.mjs", "staging-use-pipeline.mjs")
LIFECYCLE_ENV = {"GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "PATH", "STAGING_WORKER_URL",
                 "OWNERSHIP_CONTROL_TOKEN", "STAGING_USE_RECEIPT", "STAGING_USE_EXPIRES_AT",
                 "STAGING_USE_DEPLOYMENT", "STAGING_USE_SHA"}
# Hostile markers are assembled at run time so the file stays scanner-clean.
HOSTILE = "hos" + "tile" + "-policy-value-7"


def bindings(policy="bounded", env="staging", release=SHA, kind="plain_text"):
    """The finite-use binding set; a None value drops that row."""
    return [{"name": name, "type": kind, "text": value}
            for name, value in (("STAGING_RUNTIME_POLICY", policy), ("TWO_DEPLOY_ENV", env),
                                ("CONTAINER_RELEASE", release)) if value is not None]


def version_meta(version, sha=SHA, rows=None, resources=True):
    """Immutable Worker-version metadata; sha=None is a pre-stamp version."""
    data = {"id": version}
    if sha is not None:
        data["annotations"] = {"workers/message": sha}
    if resources:
        data["resources"] = {"bindings": bindings(release=sha or SHA) if rows is None else rows}
    return data


class World:
    """Minimal offline staging singleton plus its finite-use lifecycle."""

    def __init__(self, history=(PRE, TARGET, OLDER), phase="active", epoch=7,
                 ready_polls=0, running=None, meta=None):
        self.history = list(history)
        self.phase, self.epoch = phase, epoch
        self.running = phase == "active" if running is None else running
        self.ready_polls = ready_polls
        self.polls_left = 0
        self.t = 1_800_000_000.0
        self.clock = time.time  # the helper's wall clock; scripted tests use self.t
        self.calls = []
        self.posts = []
        self.never_ready = set()
        self.build = {}  # version -> served build_revision override
        self.meta = {v: version_meta(v) for v in (PRE, TARGET, OLDER)}
        self.meta.update(meta or {})
        self.events = []  # ordered ownership, lifecycle and probe events
        self.lifecycle = []  # (helper, action, env) per lifecycle invocation
        self.unexpected = []  # any other subprocess: must stay empty
        self.grant = None  # persisted (run, attempt, expiry) of the one window
        self.grant_epoch = None
        self.refuse = {}  # lifecycle action -> nth call (1-based) to refuse
        self.unavailable = set()  # lifecycle actions whose child times out
        self.control_fail = {}  # ownership action -> (nth call, code)
        self.post_failure = None
        self.post_delay = 0

    def tick(self, seconds):
        self.t += seconds

    def sha_of(self, version):
        annotations = self.meta.get(version, {}).get("annotations") or {}
        return annotations.get("workers/message")

    def actions(self, helper=None):
        return [action for name, action, _ in self.lifecycle if helper in (None, name)]


class FakeClient:
    def __init__(self, world):
        self.world = world

    def api(self, path):
        if path == DEPLOYMENTS_PATH:
            return {"deployments": [
                {"id": "d", "versions": [{"version_id": v, "percentage": 100}]}
                for v in self.world.history]}
        if path.startswith(VERSIONS_PATH) and path[len(VERSIONS_PATH):] in self.world.meta:
            return json.loads(json.dumps(self.world.meta[path[len(VERSIONS_PATH):]]))
        raise GateError("api_http_failure")

    def post(self, path, body):
        world = self.world
        world.tick(world.post_delay)
        world.posts.append((path, body))
        world.events.append(("post", body["versions"][0]["version_id"]))
        if world.post_failure:
            raise GateError("api_http_failure", world.post_failure)
        version = body["versions"][0]["version_id"]
        world.history.insert(0, version)
        return {"id": "new"}

    def request(self, url):
        world = self.world
        version = world.history[0]
        if url.endswith("/health"):
            return (200 if world.phase == "active" else 503), {}, b""
        world.events.append(("probe", version))
        up = (world.phase == "active" and world.running
              and version not in world.never_ready)
        if up and world.polls_left > 0:
            world.polls_left -= 1
            up = False
        body = json.dumps({"components": [["process", "ready"],
                                           ["gateway", "ready" if up else "starting"]],
                           "build_revision": world.build.get(version, world.sha_of(version))})
        return (200 if up else 503), {"x-two-worker-version": version}, body.encode()


def make_control(world):
    def control(action, epoch=None, release_fence=False, expected_deployment=None):
        world.tick(1)
        world.calls.append((action, epoch, release_fence, expected_deployment))
        world.events.append(("own", action))
        nth, code = world.control_fail.get(action, (0, None))
        if code and sum(1 for call in world.calls if call[0] == action) == nth:
            raise GateError(code)
        if action == "status":
            return {"owner": {"phase": world.phase, "epoch": world.epoch},
                    "running": world.running, "deploymentId": "x"}
        if action == "fence":
            if world.phase != "active" or epoch != world.epoch:
                raise GateError("ownership_control_failed")
            world.phase, world.epoch, world.running = "fenced", world.epoch + 1, False
            return {"owner": {"phase": "fenced", "epoch": world.epoch}, "running": False}
        if action == "deployment-takeover":
            if world.phase != "active" and not release_fence:
                raise GateError("ownership_control_failed")
            # Bounded policy: the new epoch stays stopped until it reacquires
            # the persisted same-run grant.
            world.phase, world.epoch, world.running = "active", world.epoch + 1, False
            world.polls_left = world.ready_polls
            return {"owner": {"phase": "active", "epoch": world.epoch}, "running": False}
        raise AssertionError(action)

    return control


def lifecycle_run(world):
    """Bounded stand-in for subprocess.run: models the finite-use helpers.

    Only the two lifecycle helpers are answered; any other command is
    recorded as unexpected and refused, so no real node or network runs.
    """
    def run(command, **kwargs):
        name = Path(str(command[1])).name if len(command) > 1 else ""
        if command[0] != "node" or name not in LIFECYCLE_HELPERS:
            world.unexpected.append(list(map(str, command)))
            raise OSError("unexpected subprocess in offline acceptance")
        action, env = command[2], dict(kwargs.get("env") or {})
        bounded = (isinstance(kwargs.get("timeout"), (int, float))
                   and 0 < kwargs["timeout"] <= 420
                   and kwargs.get("stdout") is subprocess.DEVNULL
                   and kwargs.get("stderr") is subprocess.DEVNULL)
        if not bounded or not set(env) <= LIFECYCLE_ENV:
            world.unexpected.append(["unbounded-or-leaky", name, action])
        world.lifecycle.append((name, action, env))
        world.events.append(("use", action, env.get("STAGING_USE_DEPLOYMENT")))
        if action in world.unavailable:
            raise subprocess.TimeoutExpired(command, kwargs.get("timeout"))
        nth = world.refuse.get(action)
        if nth and world.actions().count(action) == nth:
            return subprocess.CompletedProcess(command, 1)
        return subprocess.CompletedProcess(command, 0 if lifecycle_ok(world, name, action, env) else 1)

    return run


def lifecycle_ok(world, name, action, env):
    window = (env.get("GITHUB_RUN_ID"), env.get("GITHUB_RUN_ATTEMPT"),
              env.get("STAGING_USE_EXPIRES_AT"))
    if not all(window) or not env.get("STAGING_USE_RECEIPT"):
        return False
    alive = world.clock() * 1000 < int(window[2])
    if name == "staging-use-pipeline.mjs":
        # Asserts the CURRENT grant and its active owner; unavailable fenced.
        return (action == "restore-check" and window == world.grant and alive
                and world.phase == "active" and world.grant_epoch == world.epoch
                and world.running)
    version, sha = env.get("STAGING_USE_DEPLOYMENT"), env.get("STAGING_USE_SHA")
    if version != world.history[0] or sha != world.sha_of(version):
        return False
    if action == "acquire":
        if world.grant is None:
            world.grant = window  # persisted once; never minted again
            world.minted_at = world.clock()
        if window != world.grant or not alive or world.phase != "active":
            return False
        world.grant_epoch, world.running = world.epoch, True
        return True
    if action == "ready":
        return window == world.grant and alive and world.running and world.grant_epoch == world.epoch
    if action == "release":
        if window != world.grant:
            return False
        world.running = False
        return True
    return False


def environ(**changes):
    env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": TOKEN,
           "GITHUB_RUN_ID": "123456", "GITHUB_RUN_ATTEMPT": "2",
           "STAGING_USE_RECEIPT": RECEIPT, "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
           "CLOUDFLARE_API_TOKEN": CF_TOKEN}
    env.update(changes)
    return {key: value for key, value in env.items() if value is not None}


def invoke(argv, env, world):
    """Run main offline; argparse exits surface as codes, never errors.

    main's Drill runs on the wall clock, so a regression that never turns
    ready is bounded to a short real timeout here; the 300 s contract is
    asserted on the module constant and on the scripted-clock tests."""
    with tempfile.TemporaryDirectory() as directory:
        evidence = Path(directory) / "evidence.json"
        summary = Path(directory) / "summary.md"
        receipt = Path(directory) / "use.ndjson"
        env = {**env, "GITHUB_STEP_SUMMARY": str(summary)}
        if env.get("STAGING_USE_RECEIPT") is RECEIPT:
            env["STAGING_USE_RECEIPT"] = str(receipt)
        argv = list(argv) + ["--evidence", str(evidence)]
        out, err = io.StringIO(), io.StringIO()
        with patch("builtins.print",
                   lambda *a, **k: out.write(" ".join(map(str, a)) + "\n")), \
                patch.object(drill, "POLL_SECONDS", 0), \
                patch.object(drill, "READY_TIMEOUT_SECONDS", 5), \
                patch.object(subprocess, "run", lifecycle_run(world)), \
                contextlib.redirect_stderr(err):
            try:
                code = drill.main(
                    argv, environ=env,
                    make_control=lambda token, url, actor: make_control(world),
                    client_factory=lambda account, token: FakeClient(world))
            except SystemExit as done:
                code = done.code
        world.receipt_written = receipt.exists()
        return (code, out.getvalue(), err.getvalue(),
                evidence.read_text() if evidence.exists() else None, world)


def _refuse_real_subprocess(command, **_kwargs):
    raise AssertionError("offline acceptance: unpatched subprocess " + Path(str(command[1])).name)


_GUARD = patch.object(subprocess, "run", _refuse_real_subprocess)


def setUpModule():
    """No test may reach real node: every lifecycle path patches its own fake."""
    _GUARD.start()


def tearDownModule():
    _GUARD.stop()


def mutating_calls(world):
    return [call for call in world.calls if call[0] in ("fence", "deployment-takeover")]


def assert_offline(test, world, label):
    test.assertEqual(world.unexpected, [],
                     f"only bounded, redacted lifecycle helpers may run ({label})")


def assert_zero_mutations(test, world, label):
    test.assertEqual(world.posts, [], f"dry-run must never POST ({label})")
    test.assertEqual(mutating_calls(world), [],
                     f"dry-run must never fence or take over ({label})")
    test.assertEqual(world.lifecycle, [],
                     f"dry-run must never acquire, start or release ({label})")
    test.assertFalse(getattr(world, "receipt_written", False),
                     f"dry-run must never write the use receipt ({label})")
    assert_offline(test, world, label)


class DryRunPlanTests(unittest.TestCase):
    def dry_run(self, world=None, target=TARGET, env=None):
        world = world or World()
        return invoke(["--target-version", target, "--dry-run"],
                      env or environ(), world)

    def test_dry_run_exits_zero_with_stable_plan_and_zero_mutations(self):
        code, out, _err, _evidence, world = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        self.assertIn("dry-run", out, "the plan must name dry-run")
        self.assertIn("plan", out.lower(), "the plan must name the plan")
        self.assertIn(TARGET, out, "the plan must name the target version")
        self.assertIn(PRE, out, "the plan must name the serving version")
        self.assertIn("drill", out, "the plan must name the drill leg")
        self.assertIn("restore", out, "the plan must name the restore leg")
        self.assertLess(out.index("drill"), out.index("restore"),
                        "the drill leg must precede the restore leg")
        assert_zero_mutations(self, world, "valid plan")
        self.assertEqual(world.history, [PRE, TARGET, OLDER],
                         "a plan must not move the Worker")
        self.assertEqual((world.phase, world.epoch), ("active", 7),
                         "a plan must not fence ownership")
        self.assertIsNone(world.grant, "a plan must not mint a window")

    def test_dry_run_plan_is_byte_stable_across_runs(self):
        first = self.dry_run()[1]
        second = self.dry_run()[1]
        self.assertEqual(first, second,
                         "the plan must be byte-identical across runs")
        self.assertTrue(first.strip(), "the plan must not be empty")

    def test_dry_run_plan_matches_live_preflight_versions(self):
        _, live_out, _, live_evidence, live_world = invoke(
            ["--target-version", TARGET], environ(), World(ready_polls=0))
        self.assertIn("rolled back", live_out, "the live double must run")
        live = json.loads(live_evidence)
        self.assertEqual(live["failures"], [], live_out)
        self.assertEqual(live["staging_cleanup"], "release_confirmed")
        assert_offline(self, live_world, "live double")
        code, plan, _, _, _ = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        self.assertIn(live["pre_version"], plan,
                      "the plan pre version must match live preflight")
        self.assertIn(live["target_version"], plan,
                      "the plan target must match live preflight")

    def test_dry_run_reads_immutable_metadata_for_both_versions(self):
        world = World()
        reads = []
        client = FakeClient(world)
        original = client.api
        client.api = lambda path: (reads.append(path), original(path))[1]
        with patch.object(subprocess, "run", lifecycle_run(world)):
            instance = drill.Drill(client, make_control(world), URL, "1",
                                   staging_use=StagingDrillUse(
                                       environ(STAGING_USE_RECEIPT="/nonexistent/use"),
                                       URL, TOKEN))
            self.assertEqual(instance.preflight(TARGET, dry_run=True), PRE)
        for version in (PRE, TARGET):
            self.assertIn(VERSIONS_PATH + version, reads,
                          f"dry-run must read {version} bindings")
        assert_zero_mutations(self, world, "metadata read")

    def test_dry_run_refuses_idle_baseline_instead_of_claiming_startup(self):
        code, out, err, _evidence, world = self.dry_run(World(running=False))
        self.assertEqual(code, 1, "dry-run cannot preview a live startup")
        self.assertIn("singleton_not_active_and_running", out + err)
        self.assertNotIn("dry-run plan", out)
        assert_zero_mutations(self, world, "idle baseline")
        self.assertFalse(world.running, "dry-run must never start staging")


def policy_world(version, **meta):
    def factory():
        return World(meta={version: version_meta(version, **meta)})
    return factory


class DryRunRefusalParityTests(unittest.TestCase):
    def assert_parity(self, argv, env, world_factory, code, label, hostile=None):
        live_code, live_out, _, _, live_world = invoke(
            argv, env, world_factory())
        self.assertEqual(live_code, 1, f"live must refuse {label}")
        self.assertIn(code, live_out, f"live must name {code} for {label}")
        self.assertEqual(live_world.posts, [],
                         f"live refusal must not POST ({label})")
        self.assertEqual(mutating_calls(live_world), [],
                         f"live refusal must not mutate ({label})")
        self.assertEqual(live_world.lifecycle, [],
                         f"live refusal must precede acquisition ({label})")
        self.assertFalse(live_world.receipt_written,
                         f"live refusal must write no use receipt ({label})")
        assert_offline(self, live_world, label)
        dry_code, dry_out, dry_err, _, dry_world = invoke(
            [*argv, "--dry-run"], env, world_factory())
        self.assertEqual(dry_code, 1,
                         f"--dry-run must refuse {label} with exit 1")
        self.assertIn(code, dry_out + dry_err,
                      f"--dry-run must name {code} for {label}")
        assert_zero_mutations(self, dry_world, label)
        if hostile:
            for out in (live_out, dry_out + dry_err):
                self.assertNotIn(hostile, out, f"{label} must not echo metadata")

    def test_malformed_target_parity(self):
        self.assert_parity(["--target-version", "latest"], environ(),
                           World, "invalid_target_version", "malformed target")

    def test_target_is_serving_parity(self):
        self.assert_parity(["--target-version", PRE], environ(),
                           World, "target_is_serving_version",
                           "target-is-serving")

    def test_target_never_served_parity(self):
        self.assert_parity(["--target-version", NEVER], environ(),
                           World, "target_never_served_traffic",
                           "never-served target")

    def test_invalid_staging_url_parity(self):
        env = environ(STAGING_WORKER_URL="https://two-bot-next.5150.workers.dev")
        self.assert_parity(["--target-version", TARGET], env,
                           World, "invalid_staging_url", "staging url")

    def test_missing_ownership_token_parity(self):
        env = environ(OWNERSHIP_CONTROL_TOKEN="short")
        self.assert_parity(["--target-version", TARGET], env,
                           World, "missing_ownership_token", "ownership token")

    def test_invalid_run_id_parity(self):
        env = environ(GITHUB_RUN_ID="12 ; rm -rf")
        self.assert_parity(["--target-version", TARGET], env,
                           World, "invalid_run_id", "run id")

    def test_run_attempt_required_parity(self):
        for attempt in (None, "", "0", "1 ; rm -rf", "9" * 21):
            with self.subTest(attempt=attempt):
                self.assert_parity(["--target-version", TARGET],
                                   environ(GITHUB_RUN_ATTEMPT=attempt), World,
                                   "staging_drill_run_invalid", "run attempt")

    def test_use_receipt_required_parity(self):
        for receipt in (None, ""):
            with self.subTest(receipt=receipt):
                self.assert_parity(["--target-version", TARGET],
                                   environ(STAGING_USE_RECEIPT=receipt), World,
                                   "staging_drill_receipt_missing", "use receipt")

    def test_singleton_must_be_active_parity(self):
        self.assert_parity(["--target-version", TARGET], environ(),
                           lambda: World(phase="fenced"),
                           "singleton_not_active_and_running", "fenced singleton")

    def test_baseline_must_be_ready_parity(self):
        def dark():
            world = World()
            world.never_ready.add(PRE)
            return world
        self.assert_parity(["--target-version", TARGET], environ(),
                           dark, "baseline_not_ready", "dark baseline")

    def test_running_baseline_on_wrong_build_parity(self):
        def wrong_build():
            world = World()
            world.build[PRE] = OTHER_SHA
            return world
        self.assert_parity(["--target-version", TARGET], environ(),
                           wrong_build, "baseline_not_ready", "baseline build drift")

    def test_unknown_or_indefinite_target_policy_parity(self):
        cases = {
            "no resources": dict(resources=False),
            "no bindings": dict(rows=[]),
            "always-on": dict(rows=bindings(policy="always-on")),
            "missing policy": dict(rows=bindings(policy=None)),
            "production env": dict(rows=bindings(env="production")),
            "other release": dict(rows=bindings(release=OTHER_SHA)),
            "secret-typed": dict(rows=bindings(kind="secret_text")),
            "duplicate": dict(rows=bindings() + bindings()[:1]),
        }
        for label, meta in cases.items():
            with self.subTest(label):
                self.assert_parity(["--target-version", TARGET], environ(),
                                   policy_world(TARGET, **meta),
                                   "staging_restore_policy_unproved", label)

    def test_serving_version_policy_parity(self):
        self.assert_parity(["--target-version", TARGET], environ(),
                           policy_world(PRE, rows=bindings(policy="always-on")),
                           "staging_restore_policy_unproved", "serving policy")

    def test_pre_stamp_target_parity(self):
        self.assert_parity(["--target-version", TARGET], environ(),
                           policy_world(TARGET, sha=None),
                           "staging_restore_identity_invalid", "pre-stamp target")

    def test_image_mismatch_parity(self):
        self.assert_parity(["--target-version", TARGET], environ(),
                           policy_world(TARGET, sha=OTHER_SHA),
                           "staging_drill_image_mismatch", "worker-only image drift")

    def test_hostile_policy_value_never_echoes(self):
        self.assert_parity(["--target-version", TARGET], environ(),
                           policy_world(TARGET, rows=bindings(policy=HOSTILE)),
                           "staging_restore_policy_unproved", "hostile policy",
                           hostile=HOSTILE)


class LiveRefusalControls(unittest.TestCase):
    """Existing live vocabulary; green before and after the builder change."""

    def assert_live_env_refuses(self, argv, env, world, code):
        live_code, live_out, _, evidence, _ = invoke(argv, env, world)
        self.assertEqual(live_code, 1)
        self.assertIn("refused before any change", live_out)
        self.assertIn(code, live_out)
        self.assertIsNone(evidence, "an env refusal must not write evidence")
        self.assertEqual(world.lifecycle, [], "an env refusal must not acquire")

    def assert_live_preflight_refuses(self, argv, env, world, code):
        live_code, live_out, _, evidence, _ = invoke(argv, env, world)
        self.assertEqual(live_code, 1)
        self.assertIn(f"drill failed: {code}", live_out)
        self.assertEqual(json.loads(evidence)["failures"], [f"drill:{code}"])
        self.assertEqual(world.lifecycle, [], "a preflight refusal must not acquire")
        self.assertEqual(json.loads(evidence)["staging_cleanup"], "not_acquired")

    def test_live_target_classes_unchanged(self):
        for target, code in (("latest", "invalid_target_version"),
                             (PRE, "target_is_serving_version"),
                             (NEVER, "target_never_served_traffic")):
            with self.subTest(target=target):
                self.assert_live_preflight_refuses(["--target-version", target],
                                                   environ(), World(), code)

    def test_live_environment_classes_unchanged(self):
        cases = [
            (environ(STAGING_WORKER_URL="http://two-bot-next-staging.x.workers.dev"),
             "invalid_staging_url"),
            (environ(OWNERSHIP_CONTROL_TOKEN="short"), "missing_ownership_token"),
            (environ(GITHUB_RUN_ID="nope"), "invalid_run_id"),
            (environ(GITHUB_RUN_ATTEMPT=None), "staging_drill_run_invalid"),
            (environ(STAGING_USE_RECEIPT=None), "staging_drill_receipt_missing"),
        ]
        for env, code in cases:
            with self.subTest(code=code):
                self.assert_live_env_refuses(["--target-version", TARGET],
                                             env, World(), code)

    def test_live_preflight_classes_unchanged(self):
        fenced = World(phase="fenced")
        self.assert_live_preflight_refuses(["--target-version", TARGET], environ(),
                                           fenced, "singleton_not_active_and_running")
        dark = World()
        dark.never_ready.add(PRE)
        self.assert_live_preflight_refuses(["--target-version", TARGET], environ(),
                                           dark, "baseline_not_ready")

    def test_live_success_still_mutates_and_writes_evidence(self):
        code, _out, _, evidence, world = invoke(["--target-version", TARGET],
                                                environ(), World(ready_polls=0))
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(evidence)["failures"], [])
        self.assertTrue(world.posts, "live success must POST the rollback")
        self.assertIn("fence", [call[0] for call in world.calls])
        self.assertIn("deployment-takeover", [call[0] for call in world.calls])
        for _path, body in world.posts:
            self.assertNotIn("force", json.dumps(body), "a rollback never forces")
            self.assertEqual(body["code_update_strategy"], {"mode": "immediate"})
        self.assertEqual((drill.READY_BUDGET_SECONDS, drill.READY_TIMEOUT_SECONDS), (60, 300))
        self.assertFalse(world.running, "a confirmed release leaves staging stopped")


def scripted(world=None, **use_env):
    """A Drill on the world's clock with the real default lifecycle helper."""
    world = world or World()
    world.clock = lambda: world.t
    logs = []
    use = StagingDrillUse(environ(STAGING_USE_RECEIPT="/offline/use.ndjson", **use_env),
                          URL, TOKEN, now=lambda: world.t)
    instance = drill.Drill(FakeClient(world), make_control(world), URL, "123456",
                           now=lambda: world.t, sleep=world.tick, log=logs.append,
                           staging_use=use)
    return instance, world, use, logs


class FiniteWindowLifecycleTests(unittest.TestCase):
    """Live finite-window seams; every lifecycle call goes through a bounded fake."""

    def drive(self, world=None):
        instance, world, use, logs = scripted(world)
        start = world.t
        with patch.object(subprocess, "run", lifecycle_run(world)):
            failures = instance.run(TARGET)
        assert_offline(self, world, "scripted drill")
        return failures, instance, world, use, logs, start

    def index(self, world, event, after=-1):
        for position, item in enumerate(world.events):
            if position > after and item[:len(event)] == event:
                return position
        self.fail(f"{event} never happened after {after}: {world.events}")

    def test_one_grant_asserted_before_fence_and_reacquired_after_each_takeover(self):
        failures, instance, world, use, _logs, _ = self.drive()
        self.assertEqual(failures, [])
        self.assertEqual(use.expires_at, int(world.minted_at * 1000) + WINDOW_MS,
                         "begin grants exactly one 25-minute window")
        expiries = {env["STAGING_USE_EXPIRES_AT"] for _, _, env in world.lifecycle}
        attempts = {(env["GITHUB_RUN_ID"], env["GITHUB_RUN_ATTEMPT"]) for _, _, env in world.lifecycle}
        self.assertEqual(expiries, {str(use.expires_at)}, "the window is never extended")
        self.assertEqual(attempts, {("123456", "2")}, "one run/attempt owns the window")
        self.assertEqual([(name, action, env.get("STAGING_USE_DEPLOYMENT"))
                          for name, action, env in world.lifecycle], [
            ("staging-use.mjs", "acquire", PRE), ("staging-use.mjs", "ready", PRE),
            ("staging-use-pipeline.mjs", "restore-check", None),
            ("staging-use.mjs", "acquire", TARGET),
            ("staging-use.mjs", "acquire", PRE),
            ("staging-use.mjs", "release", PRE)])
        first_fence = self.index(world, ("own", "fence"))
        self.assertLess(self.index(world, ("use", "restore-check")), first_fence)
        for version in (TARGET, PRE):
            takeover = self.index(world, ("own", "deployment-takeover"),
                                  after=0 if version == TARGET else self.index(world, ("post", PRE)))
            acquire = self.index(world, ("use", "acquire", version), after=takeover)
            self.assertLess(acquire, self.index(world, ("probe", version), after=takeover),
                            "reacquire precedes the readiness loop")
        for name, _, env in world.lifecycle:
            self.assertNotIn(CF_TOKEN, json.dumps(env), "no Cloudflare credential in the child")
        self.assertEqual(instance.evidence["staging_cleanup"], "release_confirmed")
        self.assertEqual(instance.evidence["staging_expires_at"], use.expires_at)
        self.assertFalse(world.running)

    def test_idle_baseline_starts_then_must_be_exact_sha_ready(self):
        failures, instance, world, _use, _logs, _ = self.drive(World(running=False))
        self.assertEqual(failures, [])
        self.assertLess(self.index(world, ("use", "acquire", PRE)), self.index(world, ("probe", PRE)),
                        "an idle baseline is not probed before startup")

    def test_idle_baseline_on_wrong_build_refuses_and_releases(self):
        world = World(running=False)
        world.build[PRE] = OTHER_SHA
        failures, instance, world, _use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:baseline_not_ready"])
        self.assertEqual(world.posts, [])
        self.assertEqual(mutating_calls(world), [])
        self.assertEqual(world.actions()[-1], "release")
        self.assertEqual(instance.evidence["staging_cleanup"], "release_confirmed")
        self.assertFalse(world.running)

    def test_target_build_drift_times_out_then_restores_original(self):
        world = World()
        world.build[TARGET] = OTHER_SHA
        failures, instance, world, use, _logs, start = self.drive(world)
        self.assertEqual(failures, ["drill:ready_timeout"])
        self.assertEqual(world.history[0], PRE, "an ordinary timeout still restores")
        self.assertIn("first_ready", instance.evidence["restore"])
        self.assertLess(world.t * 1000, use.expires_at)
        self.assertEqual(instance.evidence["staging_cleanup"], "release_confirmed")

    def test_fenced_partial_leg_restores_without_owner_assertion(self):
        world = World()
        world.post_failure = "status=409"
        failures, instance, world, use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:api_http_failure"])
        self.assertEqual(world.actions("staging-use-pipeline.mjs"), ["restore-check"],
                         "a fenced restore never asserts the active owner")
        restore_takeover = self.index(world, ("own", "deployment-takeover"))
        acquire = self.index(world, ("use", "acquire", PRE), after=restore_takeover)
        self.assertLess(acquire, self.index(world, ("probe", PRE), after=restore_takeover))
        self.assertIn("first_ready", instance.evidence["restore"])
        self.assertEqual(world.grant, ("123456", "2", str(use.expires_at)))
        self.assertEqual(instance.evidence["staging_cleanup"], "release_confirmed")

    def test_restore_never_starts_runtime_without_reacquire(self):
        # Adversarial: if the new epoch could start without the persisted grant,
        # this world would turn ready; it must refuse instead.
        world = World()
        world.post_failure = "status=409"
        world.refuse["acquire"] = 2
        failures, instance, world, _use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:api_http_failure", "restore:staging_drill_control_failed"])
        self.assertFalse(world.running)
        self.assertNotIn("first_ready", instance.evidence["restore"])
        self.assertEqual(world.actions()[-1], "acquire", "nothing runs after a refusal")
        self.assertEqual(instance.evidence["staging_cleanup"], "native_expiry_after_control_refusal")

    def test_restore_after_original_deadline_refuses_without_extension(self):
        world = World()
        world.post_failure, world.post_delay = "status=409", 26 * 60
        failures, instance, world, use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:api_http_failure", "restore:staging_drill_window_ended"])
        self.assertEqual([c[0] for c in world.calls].count("deployment-takeover"), 0)
        self.assertEqual(len(world.posts), 1, "no restore POST after the deadline")
        self.assertEqual(world.actions(), ["acquire", "ready", "restore-check", "release"])
        self.assertEqual({env["STAGING_USE_EXPIRES_AT"] for _, _, env in world.lifecycle},
                         {str(use.expires_at)})

    def test_window_ended_before_drill_refuses_before_fence(self):
        instance, world, use, _logs = scripted()
        with patch.object(subprocess, "run", lifecycle_run(world)):
            instance.preflight(TARGET)
            world.tick(25 * 60)
            with self.assertRaises(GateError) as caught:
                instance.leg("drill", TARGET)
        self.assertEqual(str(caught.exception), "staging_drill_window_ended")
        self.assertEqual(mutating_calls(world), [])
        self.assertEqual(world.posts, [])

    def test_refused_restore_check_stops_before_any_mutation(self):
        world = World()
        world.refuse["restore-check"] = 1
        failures, instance, world, _use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:staging_drill_control_failed"])
        self.assertEqual(world.posts, [])
        self.assertEqual(mutating_calls(world), [])
        self.assertEqual(world.actions()[-1], "restore-check")
        self.assertEqual(instance.evidence["staging_cleanup"], "native_expiry_after_control_refusal")

    def test_lifecycle_refusal_stops_restore_and_cleanup(self):
        world = World()
        world.refuse["acquire"] = 2  # the drill leg's rebind
        failures, instance, world, _use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:staging_drill_control_failed",
                                    "restore:staging_drill_window_ended"])
        self.assertEqual(len(world.posts), 1, "no restore POST after a lifecycle refusal")
        self.assertEqual([c[0] for c in world.calls].count("deployment-takeover"), 1)
        self.assertEqual(world.actions()[-1], "acquire")
        self.assertEqual(instance.evidence["staging_cleanup"], "native_expiry_after_control_refusal")

    def test_lifecycle_unavailable_stops_without_mutation(self):
        world = World()
        world.unavailable.add("restore-check")
        failures, instance, world, _use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:staging_drill_control_unavailable"])
        self.assertEqual(mutating_calls(world), [])
        self.assertNotIn("release", world.actions())
        self.assertEqual(instance.evidence["staging_cleanup"], "native_expiry_after_control_refusal")

    def test_authentication_failure_stops_restore_and_release(self):
        world = World()
        world.control_fail["fence"] = (1, "ownership_auth_failed")
        failures, instance, world, _use, logs, _ = self.drive(world)
        self.assertEqual(failures, ["drill:ownership_auth_failed",
                                    "restore_skipped_after_auth_failure"])
        self.assertEqual(world.posts, [])
        self.assertEqual(world.actions()[-1], "restore-check", "no control after auth failure")
        self.assertTrue(any("never substitute credentials" in line for line in logs))
        self.assertEqual(instance.evidence["staging_cleanup"],
                         "native_expiry_after_control_refusal")

    def test_restore_leg_authentication_failure_skips_release(self):
        world = World()
        world.control_fail["deployment-takeover"] = (2, "ownership_auth_failed")
        failures, instance, world, _use, _logs, _ = self.drive(world)
        self.assertEqual(failures, ["restore:ownership_auth_failed"])
        self.assertEqual(world.actions()[-1], "acquire", "no release after auth failure")
        self.assertNotIn("release", world.actions())
        self.assertEqual(instance.evidence["staging_cleanup"],
                         "native_expiry_after_control_refusal")

    def test_helper_never_releases_or_reasserts_after_a_refusal(self):
        # Defense in depth inside the default helper itself, not only Drill.run.
        instance, world, use, _logs = scripted()
        world.refuse["ready"] = 1
        use.validate(FakeClient(world), (PRE, TARGET))
        with patch.object(subprocess, "run", lifecycle_run(world)):
            with self.assertRaises(GateError) as refused:
                use.begin(PRE)
            self.assertEqual(str(refused.exception), "staging_drill_control_failed")
            self.assertTrue(use.failed)
            use.finish()
            with self.assertRaises(GateError) as caught:
                use.check()
            self.assertEqual(str(caught.exception), "staging_drill_window_ended")
            self.assertEqual(world.actions(), ["acquire", "ready"])
            with self.assertRaises(GateError) as again:
                use.begin(PRE)  # one grant per run attempt; never re-minted
        self.assertEqual(str(again.exception), "staging_drill_window_already_started")
        self.assertEqual(world.actions(), ["acquire", "ready"])

    def test_unconfirmed_release_fails_nonzero_without_claiming_idle(self):
        world = World()
        world.refuse["release"] = 1
        code, out, _err, evidence, world = invoke(["--target-version", TARGET], environ(), world)
        self.assertEqual(code, 1, out)
        receipt = json.loads(evidence)
        self.assertEqual(receipt["failures"], ["cleanup:staging_drill_control_failed"])
        self.assertEqual(receipt["staging_cleanup"], "native_expiry_release_unconfirmed")
        self.assertNotIn("release_confirmed", out)
        self.assertNotIn("idle", out)
        assert_offline(self, world, "release refusal")


if __name__ == "__main__":
    unittest.main()

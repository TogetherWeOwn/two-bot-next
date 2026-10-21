#!/usr/bin/env python3
"""Acceptance tests: staging rollback drill --dry-run previews the plan.

DR (staging drill dry-run). Offline only: stubbed Cloudflare client and
ownership control, no network, no subprocess, no credentials, no databases.

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
3. Plan/live pre-mutation parity: every invalid input class refuses with the
   identical code in ``--dry-run`` and live mode, before any mutation. The
   evidence file shape for ``--dry-run`` is unspecified and not asserted.
4. RED is behavioral: every failing assertion below fails on an exit code,
   plan-text or call-count comparison against live code paths, never on an
   import error, missing fixture or refusal to run. ``--dry-run`` is
   missing today, so argparse exits 2 and the dry-run assertions fail.

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

PRE = "11111111-1111-4111-8111-111111111111"
TARGET = "22222222-2222-4222-8222-222222222222"
OLDER = "33333333-3333-4333-8333-333333333333"
NEVER = "44444444-4444-4444-8444-444444444444"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
ACCOUNT = "b" * 32
TOKEN = "t" * 40
DEPLOYMENTS_PATH = "/workers/scripts/two-bot-next-staging/deployments"


class World:
    """Minimal offline staging singleton for dry-run acceptance."""

    def __init__(self, history=(PRE, TARGET, OLDER), phase="active", epoch=7,
                 ready_polls=0):
        self.history = list(history)
        self.phase, self.epoch = phase, epoch
        self.running = phase == "active"
        self.ready_polls = ready_polls
        self.polls_left = 0
        self.t = 1_800_000_000.0
        self.calls = []
        self.posts = []
        self.never_ready = set()

    def tick(self, seconds):
        self.t += seconds


class FakeClient:
    def __init__(self, world):
        self.world = world

    def api(self, path):
        if path == DEPLOYMENTS_PATH:
            return {"deployments": [
                {"id": "d", "versions": [{"version_id": v, "percentage": 100}]}
                for v in self.world.history]}
        raise GateError("api_http_failure")

    def post(self, path, body):
        self.world.posts.append((path, body))
        version = body["versions"][0]["version_id"]
        self.world.history.insert(0, version)
        return {"id": "new"}

    def request(self, url):
        world = self.world
        version = world.history[0]
        if url.endswith("/health"):
            return (200 if world.phase == "active" else 503), {}, b""
        up = (world.phase == "active" and world.running
              and version not in world.never_ready)
        if up and world.polls_left > 0:
            world.polls_left -= 1
            up = False
        body = json.dumps({"components": [["process", "ready"],
                                           ["gateway", "ready" if up else "starting"]]})
        return (200 if up else 503), {"x-two-worker-version": version}, body.encode()


def make_control(world):
    def control(action, epoch=None, release_fence=False, expected_deployment=None):
        world.tick(1)
        world.calls.append((action, epoch, release_fence, expected_deployment))
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
            world.phase, world.epoch, world.running = "active", world.epoch + 1, True
            world.polls_left = world.ready_polls
            return {"owner": {"phase": "active", "epoch": world.epoch}, "running": True}
        raise AssertionError(action)

    return control


def environ(**changes):
    env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": TOKEN,
           "GITHUB_RUN_ID": "123456", "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
           "CLOUDFLARE_API_TOKEN": TOKEN}
    env.update(changes)
    return env


def invoke(argv, env, world):
    """Run main offline; argparse exits surface as codes, never errors."""
    with tempfile.TemporaryDirectory() as directory:
        evidence = Path(directory) / "evidence.json"
        summary = Path(directory) / "summary.md"
        env = {**env, "GITHUB_STEP_SUMMARY": str(summary)}
        argv = list(argv) + ["--evidence", str(evidence)]
        out, err = io.StringIO(), io.StringIO()
        with patch("builtins.print",
                   lambda *a, **k: out.write(" ".join(map(str, a)) + "\n")), \
                patch.object(drill, "POLL_SECONDS", 0), \
                contextlib.redirect_stderr(err):
            try:
                code = drill.main(
                    argv, environ=env,
                    make_control=lambda token, url, actor: make_control(world),
                    client_factory=lambda account, token: FakeClient(world))
            except SystemExit as done:
                code = done.code
        return (code, out.getvalue(), err.getvalue(),
                evidence.read_text() if evidence.exists() else None, world)


def mutating_calls(world):
    return [call for call in world.calls if call[0] in ("fence", "deployment-takeover")]


def assert_zero_mutations(test, world, label):
    test.assertEqual(world.posts, [], f"dry-run must never POST ({label})")
    test.assertEqual(mutating_calls(world), [],
                     f"dry-run must never fence or take over ({label})")


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

    def test_dry_run_plan_is_byte_stable_across_runs(self):
        first = self.dry_run()[1]
        second = self.dry_run()[1]
        self.assertEqual(first, second,
                         "the plan must be byte-identical across runs")
        self.assertTrue(first.strip(), "the plan must not be empty")

    def test_dry_run_plan_matches_live_preflight_versions(self):
        _, live_out, _, live_evidence, _ = invoke(
            ["--target-version", TARGET], environ(), World(ready_polls=0))
        self.assertIn("rolled back", live_out, "the live double must run")
        live = json.loads(live_evidence)
        code, plan, _, _, _ = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        self.assertIn(live["pre_version"], plan,
                      "the plan pre version must match live preflight")
        self.assertIn(live["target_version"], plan,
                      "the plan target must match live preflight")


class DryRunRefusalParityTests(unittest.TestCase):
    def assert_parity(self, argv, env, world_factory, code, label):
        live_code, live_out, _, _, live_world = invoke(
            argv, env, world_factory())
        self.assertEqual(live_code, 1, f"live must refuse {label}")
        self.assertIn(code, live_out, f"live must name {code} for {label}")
        self.assertEqual(live_world.posts, [],
                         f"live refusal must not POST ({label})")
        self.assertEqual(mutating_calls(live_world), [],
                         f"live refusal must not mutate ({label})")
        dry_code, dry_out, dry_err, _, dry_world = invoke(
            [*argv, "--dry-run"], env, world_factory())
        self.assertEqual(dry_code, 1,
                         f"--dry-run must refuse {label} with exit 1")
        self.assertIn(code, dry_out + dry_err,
                      f"--dry-run must name {code} for {label}")
        assert_zero_mutations(self, dry_world, label)

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


class LiveRefusalControls(unittest.TestCase):
    """Existing live vocabulary; green before and after the builder change."""

    def assert_live_env_refuses(self, argv, env, world, code):
        live_code, live_out, _, evidence, _ = invoke(argv, env, world)
        self.assertEqual(live_code, 1)
        self.assertIn("refused before any change", live_out)
        self.assertIn(code, live_out)
        self.assertIsNone(evidence, "an env refusal must not write evidence")

    def assert_live_preflight_refuses(self, argv, env, world, code):
        live_code, live_out, _, evidence, _ = invoke(argv, env, world)
        self.assertEqual(live_code, 1)
        self.assertIn(f"drill failed: {code}", live_out)
        self.assertEqual(json.loads(evidence)["failures"], [f"drill:{code}"])

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


if __name__ == "__main__":
    unittest.main()

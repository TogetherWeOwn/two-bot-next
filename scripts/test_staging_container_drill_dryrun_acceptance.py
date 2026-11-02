#!/usr/bin/env python3
"""Acceptance tests: staging container drill --dry-run previews the plan.

DR (staging drill dry-run). Offline only: stubbed Cloudflare client,
ownership control and wrangler deploy, no network, no subprocess, no
credentials, no databases.

Contract under test
-------------------
1. ``staging_container_drill.py`` with the eight ``--backout-*`` pins plus
   ``--dry-run`` and a valid environment exits 0, prints a stable plan to
   stdout, and performs zero mutations. Stable means byte-identical stdout
   across two runs (no wall-clock). The plan names ``dry-run`` and
   ``plan``, the backout image, worker version and source SHA, and both
   legs ``drill`` then ``restore`` in order.
2. Zero mutations means the wrangler deploy closure is never invoked and
   the control is never called with ``fence`` or ``deployment-takeover``
   (``status`` reads are allowed). Serving image, history and phase are
   unchanged.
3. Plan/live pre-mutation parity: every invalid pin, environment and
   preflight class refuses with the identical code in ``--dry-run`` and
   live mode, before any mutation. The evidence and deploy-config shapes
   for ``--dry-run`` are unspecified and not asserted.
4. RED is behavioral: every failing assertion fails on an exit code,
   plan-text or call-count comparison, never on an import error, missing
   fixture or refusal to run. ``--dry-run`` is missing today, so argparse
   exits 2 and the dry-run assertions fail.

Controls assert the existing live refusal vocabulary is unchanged; they
pass before and after the builder's change.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_container_drill_dryrun_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "staging_container_drill_dryrun_under_test",
    Path(__file__).with_name("staging_container_drill.py"))
container_drill = importlib.util.module_from_spec(spec)
spec.loader.exec_module(container_drill)
GateError = container_drill.GateError

PRE = "11111111-1111-4111-8111-111111111111"
BACKOUT_WORKER = "22222222-2222-4222-8222-222222222222"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
ACCOUNT = "b" * 32
TOKEN = "t" * 40
APP = "two-bot-next-twobotcontainer-staging"
REPO = "f" * 16
PRE_IMAGE = f"registry.cloudflare.com/{REPO}/{APP}@sha256:{'a' * 64}"
BACKOUT_IMAGE = f"registry.cloudflare.com/{REPO}/{APP}@sha256:{'b' * 64}"
PRE_SHA = "a" * 40
BACKOUT_SHA = "b" * 40
PRE_BUILD = "100-1"
BACKOUT_BUILD = "99-1"
DEPLOYMENTS_PATH = "/workers/scripts/two-bot-next-staging/deployments"
APPLICATIONS_PATH = "/containers/applications"
ROLLOUTS_PATH = "/containers/applications/app1/rollouts?limit=100"
COVERING_ATTESTATION = json.dumps({
    "drill": {"distinct_sessions": 1, "window_start": "1970-01-01T00:00:00Z",
              "window_end": "2286-11-20T17:46:39Z"},
    "restore": {"distinct_sessions": 1, "window_start": "1970-01-01T00:00:00Z",
                "window_end": "2286-11-20T17:46:39Z"}})
WRANGLER_TOML = """\
name = "two-bot-next"
main = "src/index.ts"

[[containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile"
max_instances = 1

[env.staging.version_metadata]
binding = "CF_VERSION_METADATA"

[[env.staging.containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile"
max_instances = 1
"""


def pin(**changes):
    base = {"source_sha": BACKOUT_SHA, "build_id": BACKOUT_BUILD,
            "image": BACKOUT_IMAGE, "worker_version": BACKOUT_WORKER,
            "rollout_id": "r-backout", "review_ref": "PR-612:ci-ok-9ed49294",
            "staging_run_id": "37243860909",
            "compatibility_note": "schema-0417+DO-unchanged+flags-unchanged"}
    base.update(changes)
    return base


class World:
    def __init__(self, history=(PRE, BACKOUT_WORKER), phase="active", epoch=7,
                 ready_polls=0):
        self.history = list(history)
        self.phase, self.epoch = phase, epoch
        self.running = phase == "active"
        self.serving_image = PRE_IMAGE
        self.rollouts = [
            {"id": "r-pre", "created_at": "2026-10-04T21:00:00Z",
             "status": "completed",
             "target_configuration": {"image": PRE_IMAGE},
             "target_version": 41, "strategy": "rolling", "kind": "full_auto",
             "current_version": 40, "current_configuration": {"image": PRE_IMAGE},
             "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                      "starting": 0, "scheduling": 0}},
             "progress": {"total_steps": 1, "current_step": 1,
                          "total_instances": 1, "updated_instances": 1},
             "steps": [{"status": "completed"}]},
            {"id": "r-backout", "created_at": "2026-10-03T21:00:00Z",
             "status": "completed",
             "target_configuration": {"image": BACKOUT_IMAGE},
             "target_version": 40, "strategy": "rolling", "kind": "full_auto",
             "current_version": 39,
             "current_configuration": {"image": BACKOUT_IMAGE},
             "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                      "starting": 0, "scheduling": 0}},
             "progress": {"total_steps": 1, "current_step": 1,
                          "total_instances": 1, "updated_instances": 1},
             "steps": [{"status": "completed"}]},
        ]
        self.namespaces = {PRE: "ns1", BACKOUT_WORKER: "ns1"}
        self.ready_polls = ready_polls
        self.polls_left = 0
        self.never_ready = set()
        self.t = 1_800_000_000.0
        self.calls = []
        self.deploys = []
        self.messages = []

    def tick(self, seconds):
        self.t += seconds

    def revision_for(self, image):
        if image == BACKOUT_IMAGE:
            return BACKOUT_SHA, BACKOUT_BUILD
        if image == PRE_IMAGE:
            return PRE_SHA, PRE_BUILD
        return "unknown", "unknown"


class FakeClient:
    def __init__(self, world):
        self.world = world

    def api(self, path):
        world = self.world
        if path == DEPLOYMENTS_PATH:
            return {"deployments": [
                {"id": "d", "versions": [{"version_id": v, "percentage": 100}]}
                for v in world.history]}
        if path == APPLICATIONS_PATH:
            return [{"id": "app1", "name": APP,
                     "durable_objects": {"namespace_id": "ns1"},
                     "configuration": {"image": world.serving_image}}]
        if path == ROLLOUTS_PATH:
            return [dict(row) for row in world.rollouts]
        if path.startswith("/containers/applications/app1/rollouts/"):
            rid = path.rsplit("/", 1)[1]
            matches = [row for row in world.rollouts if row["id"] == rid]
            if not matches:
                raise GateError("api_failure")
            return dict(matches[0])
        if path.startswith("/workers/scripts/two-bot-next-staging/versions/"):
            version = path.rsplit("/", 1)[1]
            namespace = world.namespaces.get(version)
            if namespace is None:
                return {"resources": {"bindings": []}}
            return {"resources": {"bindings": [
                {"type": "durable_object_namespace", "class_name": "TwoBotContainer",
                 "name": "TWO_BOT", "namespace_id": namespace}]}}
        raise AssertionError(path)

    def request(self, url):
        world = self.world
        version = world.history[0]
        if url.endswith("/health"):
            return (200 if world.phase == "active" else 503), {}, b""
        up = (world.phase == "active" and world.running
              and world.serving_image not in world.never_ready)
        if up and world.polls_left > 0:
            world.polls_left -= 1
            up = False
        revision, build = world.revision_for(world.serving_image)
        body = json.dumps({"components": [["process", "ready"],
                                           ["gateway", "ready" if up else "starting"],
                                           ["database", "ready"]],
                           "build_revision": revision, "build_id": build})
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


def rollout_row(world, rid, image):
    return {"id": rid, "created_at": container_drill.iso(world.t),
            "status": "completed",
            "target_configuration": {"image": image}, "target_version": 42,
            "strategy": "rolling", "kind": "full_auto",
            "current_version": 41, "current_configuration": {"image": image},
            "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                     "starting": 0, "scheduling": 0}},
            "progress": {"total_steps": 1, "current_step": 1,
                         "total_instances": 1, "updated_instances": 1},
            "steps": [{"status": "completed"}]}


def environ(**changes):
    env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": TOKEN,
           "GITHUB_RUN_ID": "123456", "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
           "CLOUDFLARE_API_TOKEN": TOKEN}
    env.update(changes)
    return env


def argv_for(directory, raw_pin, attestation=COVERING_ATTESTATION):
    argv = []
    for key in ("source_sha", "build_id", "image", "worker_version",
                "rollout_id", "review_ref", "staging_run_id"):
        argv += [f"--backout-{key.replace('_', '-')}", raw_pin[key]]
    argv += ["--compatibility-note", raw_pin["compatibility_note"],
            "--session-attestation", attestation,
            "--evidence", str(Path(directory) / "evidence.json"),
            "--deploy-config", str(Path(directory) / "deploy.json")]
    return argv


def invoke_with_pin(raw_pin, env, world, extra=()):
    with tempfile.TemporaryDirectory() as directory:
        wrangler_dir = Path(directory) / "wrangler"
        wrangler_dir.mkdir()
        (wrangler_dir / "wrangler.toml").write_text(WRANGLER_TOML)
        evidence = Path(directory) / "evidence.json"
        summary = Path(directory) / "summary.md"
        env = {**env, "GITHUB_STEP_SUMMARY": str(summary)}
        argv = argv_for(directory, raw_pin) + list(extra) + ["--wrangler-dir", str(wrangler_dir)]

        def deploy_factory(_bin, _dir, _env):
            def deploy(config_path, message_sha):
                config = json.loads(Path(config_path).read_text())
                image = config["env"]["staging"]["containers"][0]["image"]
                world.tick(2)
                world.deploys.append(image)
                world.messages.append(message_sha)
                world.serving_image = image
                world.rollouts.append(rollout_row(world, f"r-{len(world.rollouts)}", image))
                return 0
            return deploy

        out, err = io.StringIO(), io.StringIO()
        with patch("builtins.print",
                   lambda *a, **k: out.write(" ".join(map(str, a)) + "\n")), \
                patch.object(container_drill, "POLL_SECONDS", 0), \
                contextlib.redirect_stderr(err):
            try:
                code = container_drill.main(
                    argv, environ=env,
                    make_control=lambda token, url, actor: make_control(world),
                    client_factory=lambda account, token: FakeClient(world),
                    deploy_factory=deploy_factory)
            except SystemExit as done:
                code = done.code
        return (code, out.getvalue(), err.getvalue(),
                evidence.read_text() if evidence.exists() else None, world)


def mutating_calls(world):
    return [call for call in world.calls if call[0] in ("fence", "deployment-takeover")]


def assert_zero_mutations(test, world, label):
    test.assertEqual(world.deploys, [], f"dry-run must never deploy ({label})")
    test.assertEqual(mutating_calls(world), [],
                     f"dry-run must never fence or take over ({label})")


class DryRunPlanTests(unittest.TestCase):
    def dry_run(self, raw_pin=None, env=None, world=None):
        return invoke_with_pin(raw_pin or pin(), env or environ(),
                               world or World(), extra=("--dry-run",))

    def test_dry_run_exits_zero_with_stable_plan_and_zero_mutations(self):
        code, out, _err, _evidence, world = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        self.assertIn("dry-run", out, "the plan must name dry-run")
        self.assertIn("plan", out.lower(), "the plan must name the plan")
        self.assertIn(BACKOUT_IMAGE, out, "the plan must name the backout image")
        self.assertIn(BACKOUT_WORKER, out, "the plan must name the backout worker")
        self.assertIn(BACKOUT_SHA, out, "the plan must name the backout source")
        self.assertIn("drill", out, "the plan must name the drill leg")
        self.assertIn("restore", out, "the plan must name the restore leg")
        self.assertLess(out.index("drill"), out.index("restore"),
                        "the drill leg must precede the restore leg")
        assert_zero_mutations(self, world, "valid plan")
        self.assertEqual(world.serving_image, PRE_IMAGE,
                         "a plan must not move the serving image")
        self.assertEqual((world.phase, world.epoch), ("active", 7),
                         "a plan must not fence ownership")

    def test_dry_run_plan_is_byte_stable_across_runs(self):
        first = self.dry_run()[1]
        second = self.dry_run()[1]
        self.assertEqual(first, second,
                         "the plan must be byte-identical across runs")
        self.assertTrue(first.strip(), "the plan must not be empty")

    def test_dry_run_plan_matches_live_backout_pins(self):
        _, _, _, live_evidence, _ = invoke_with_pin(
            pin(), environ(), World(ready_polls=0))
        live = json.loads(live_evidence)
        code, plan, _, _, _ = self.dry_run()
        self.assertEqual(code, 0, "a valid --dry-run must exit 0 with a plan")
        backout = live["backout"]
        for key in ("source_sha", "build_id", "image", "worker_version"):
            with self.subTest(key=key):
                self.assertIn(backout[key], plan,
                              "the plan must match the live backout pins")


class DryRunRefusalParityTests(unittest.TestCase):
    def assert_parity(self, raw_pin, env, world_factory, code, label):
        live_code, live_out, _, _, live_world = invoke_with_pin(
            raw_pin, env, world_factory())
        self.assertEqual(live_code, 1, f"live must refuse {label}")
        self.assertIn(code, live_out, f"live must name {code} for {label}")
        self.assertEqual(live_world.deploys, [],
                         f"live refusal must not deploy ({label})")
        self.assertEqual(mutating_calls(live_world), [],
                         f"live refusal must not mutate ({label})")
        dry_code, dry_out, dry_err, _, dry_world = invoke_with_pin(
            raw_pin, env, world_factory(), extra=("--dry-run",))
        self.assertEqual(dry_code, 1,
                         f"--dry-run must refuse {label} with exit 1")
        self.assertIn(code, dry_out + dry_err,
                      f"--dry-run must name {code} for {label}")
        assert_zero_mutations(self, dry_world, label)

    def test_each_invalid_pin_class_parity(self):
        cases = [
            (pin(source_sha="latest"), "invalid_source_sha", "source sha"),
            (pin(build_id="latest"), "invalid_build_id", "build id"),
            (pin(image=BACKOUT_IMAGE.replace("@sha256:", ":")),
             "invalid_image_digest", "image digest"),
            (pin(worker_version="latest"), "invalid_worker_version",
             "worker version"),
            (pin(rollout_id="r backout"), "invalid_rollout_id", "rollout id"),
            (pin(review_ref="see body for review"), "invalid_review_ref",
             "review ref"),
            (pin(staging_run_id="latest"), "invalid_run_id", "staging run id"),
            (pin(compatibility_note="schema changed; see doc"),
             "invalid_compatibility_note", "compatibility note"),
        ]
        for raw, code, label in cases:
            with self.subTest(label=label):
                self.assert_parity(raw, environ(), World, code, label)

    def test_invalid_environment_parity(self):
        for env, code, label in [
                (environ(STAGING_WORKER_URL="https://two-bot-next.5150.workers.dev"),
                 "invalid_staging_url", "staging url"),
                (environ(OWNERSHIP_CONTROL_TOKEN="short"),
                 "missing_ownership_token", "ownership token"),
                (environ(GITHUB_RUN_ID="12 ; rm -rf"),
                 "invalid_run_id", "run id")]:
            with self.subTest(label=label):
                self.assert_parity(pin(), env, World, code, label)

    def test_preflight_parity(self):
        self.assert_parity(pin(image=PRE_IMAGE), environ(), World,
                           "same_image_as_serving", "same image")
        self.assert_parity(pin(rollout_id="r-missing"), environ(), World,
                           "backout_rollout_absent", "absent rollout")
        self.assert_parity(pin(), environ(), lambda: World(phase="fenced"),
                           "singleton_not_active_and_running", "fenced singleton")

        def dark():
            world = World()
            world.never_ready.add(PRE_IMAGE)
            return world
        self.assert_parity(pin(), environ(), dark,
                           "baseline_not_ready", "dark baseline")


class LiveRefusalControls(unittest.TestCase):
    """Existing live vocabulary; green before and after the builder change."""

    def assert_live_refuses(self, raw_pin, env, world, code):
        live_code, live_out, _, evidence, _ = invoke_with_pin(raw_pin, env, world)
        self.assertEqual(live_code, 1)
        self.assertIn("refused before any change", live_out)
        self.assertIn(code, live_out)
        self.assertIsNone(evidence, "a pin/env refusal must not write evidence")

    def assert_live_preflight_refuses(self, raw_pin, env, world, code):
        live_code, live_out, _, evidence, _ = invoke_with_pin(raw_pin, env, world)
        self.assertEqual(live_code, 1)
        self.assertIn(f"drill failed: {code}", live_out)
        self.assertEqual(json.loads(evidence)["failures"], [f"drill:{code}"])

    def test_live_pin_classes_unchanged(self):
        for raw, code in [
                (pin(source_sha="latest"), "invalid_source_sha"),
                (pin(build_id="latest"), "invalid_build_id"),
                (pin(worker_version="latest"), "invalid_worker_version"),
                (pin(rollout_id="r backout"), "invalid_rollout_id")]:
            with self.subTest(code=code):
                self.assert_live_refuses(raw, environ(), World(), code)

    def test_live_environment_classes_unchanged(self):
        cases = [
            (environ(STAGING_WORKER_URL="http://two-bot-next-staging.x.workers.dev"),
             "invalid_staging_url"),
            (environ(OWNERSHIP_CONTROL_TOKEN="short"), "missing_ownership_token"),
            (environ(GITHUB_RUN_ID="nope"), "invalid_run_id"),
        ]
        for env, code in cases:
            with self.subTest(code=code):
                self.assert_live_refuses(pin(), env, World(), code)

    def test_live_preflight_classes_unchanged(self):
        self.assert_live_preflight_refuses(pin(image=PRE_IMAGE), environ(), World(),
                                           "same_image_as_serving")
        self.assert_live_preflight_refuses(pin(rollout_id="r-missing"), environ(), World(),
                                           "backout_rollout_absent")

    def test_live_success_still_deploys_and_writes_evidence(self):
        code, _out, _, evidence, world = invoke_with_pin(
            pin(), environ(), World(ready_polls=0))
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(evidence)["failures"], [])
        self.assertTrue(world.deploys, "live success must deploy the image")
        self.assertIn("fence", [call[0] for call in world.calls])
        self.assertIn("deployment-takeover", [call[0] for call in world.calls])


if __name__ == "__main__":
    unittest.main()

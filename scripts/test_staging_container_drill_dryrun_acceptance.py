#!/usr/bin/env python3
"""Acceptance tests: staging container drill --dry-run previews the plan.

DR (staging drill dry-run). Offline only: stubbed Cloudflare client,
ownership control and wrangler deploy, and the real ``StagingDrillUse``
with only its lifecycle transport (the ``node staging-use*.mjs`` child)
replaced by a recorder. No network, no subprocess, no credentials, no
databases.

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

Finite staging use (on-demand lifecycle)
----------------------------------------
5. The run needs a real run attempt and a ``STAGING_USE_RECEIPT``; both
   modes refuse a missing/invalid one before any lifecycle call, and the
   receipt never reaches stdout or evidence.
6. Baseline and historical backout versions must carry immutable
   finite-policy metadata (``id``, ``workers/message`` annotation and
   ``plain_text`` ``STAGING_RUNTIME_POLICY=bounded``,
   ``TWO_DEPLOY_ENV=staging``, ``CONTAINER_RELEASE=<sha>`` bindings) that
   names the pinned SHA; a mismatch refuses before acquisition in both modes.
7. ``--dry-run`` makes zero lifecycle calls and refuses an idle baseline;
   live use acquires exactly one 25-minute grant for the run/attempt and
   starts an idle baseline only after that proof.
8. Each full deploy creates a NEW version UUID. It is metadata, SHA and
   namespace proved before takeover, takeover names the new UUID (never
   the historical pin), and the original grant is reacquired for it with
   the original expiry unchanged. A metadata mismatch refuses before
   takeover and stops every later control and restore.
9. Expiry is enforced through readiness and confirmation and before the
   restore leg; a fenced partial restore never asserts an active owner.
10. A lifecycle refusal stops later controls and restore and leaves
    teardown to native expiry; cleanup must confirm release or report
    ``native_expiry_release_unconfirmed``.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_container_drill_dryrun_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "staging_container_drill_dryrun_under_test",
    Path(__file__).with_name("staging_container_drill.py"))
container_drill = importlib.util.module_from_spec(spec)
spec.loader.exec_module(container_drill)
GateError = container_drill.GateError
# The drill module imports its finite-use adapter itself; bind that exact
# module object so the transport patch lands where the drill looks it up.
staging_drill_use = sys.modules[container_drill.StagingDrillUse.__module__]

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
NEW_WORKER = "33333333-3333-4333-8333-{:012d}"
ATTEMPT = "2"
RECEIPT = "SENTINEL_STAGING_RECEIPT_8c1e2f"
WINDOW_MS = 25 * 60 * 1000
POLICY = {"STAGING_RUNTIME_POLICY": "bounded", "TWO_DEPLOY_ENV": "staging"}
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
                 ready_polls=0, idle=False):
        self.history = list(history)
        self.phase, self.epoch = phase, epoch
        self.running = phase == "active" and not idle
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
        # Immutable version metadata: workers/message annotation plus the
        # plain_text policy bindings the version was deployed with.
        self.metadata = {PRE: {"sha": PRE_SHA, "vars": dict(POLICY, CONTAINER_RELEASE=PRE_SHA)},
                         BACKOUT_WORKER: {"sha": BACKOUT_SHA,
                                          "vars": dict(POLICY, CONTAINER_RELEASE=BACKOUT_SHA)}}
        self.version_ids = {}  # version -> reported id (identity drift)
        self.new_versions = []
        self.corrupt_new = None  # callable(meta) mutating the next new version
        self.refuse_deploy = set()  # images whose deploy exits non-zero
        self.lifecycle = []  # recorded staging-use child invocations
        self.refuse_lifecycle = {}  # call index -> "refused" | "unavailable"
        self.events = []  # ordered control/deploy/lifecycle actions
        self.probes = 0
        self.jump_at_probe = None
        self.jump_on_deploy = None  # image whose deploy outlasts the grant
        self.jump_on_app_read_after_probe = None
        self.jumped = None
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
            after = world.jump_on_app_read_after_probe
            if after is not None and world.probes >= after:
                # The grant ends between confirmation and the image re-read.
                world.jump_on_app_read_after_probe = None
                world.tick(WINDOW_MS / 1000)
                world.jumped = (len(world.events), len(world.deploys))
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
            meta = world.metadata.get(version, {})
            bindings = [{"type": "plain_text", "name": name, "text": text}
                        for name, text in meta.get("vars", {}).items()]
            if namespace is not None:
                bindings.append(
                    {"type": "durable_object_namespace", "class_name": "TwoBotContainer",
                     "name": "TWO_BOT", "namespace_id": namespace})
            return {"id": world.version_ids.get(version, version),
                    "annotations": {"workers/message": meta.get("sha")},
                    "resources": {"bindings": bindings}}
        raise AssertionError(path)

    def request(self, url):
        world = self.world
        version = world.history[0]
        if url.endswith("/health"):
            return (200 if world.phase == "active" else 503), {}, b""
        world.probes += 1
        if world.probes == world.jump_at_probe:
            # The wall clock crosses the grant deadline mid-wait.
            world.tick(WINDOW_MS / 1000)
            world.jumped = (len(world.events), len(world.deploys))
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
        world.events.append(("control", action, expected_deployment))
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


def lifecycle_transport(world):
    """Replaces only `subprocess` inside staging_drill_use: records each
    `node wrangler/scripts/staging-use*.mjs <action>` child, never runs it."""
    def run(argv, *, env, stdout, stderr, timeout, check):
        assert argv[0] == "node" and len(argv) == 3, argv
        helper = Path(argv[1])
        assert helper.parent.parts[-2:] == ("wrangler", "scripts"), argv
        assert helper.name in ("staging-use.mjs", "staging-use-pipeline.mjs"), argv
        assert (stdout, stderr, check) == (subprocess.DEVNULL, subprocess.DEVNULL, False)
        called_at = world.t
        world.tick(1)
        index = len(world.lifecycle)
        world.lifecycle.append({
            "t": called_at,
            "action": argv[2], "pipeline": helper.name == "staging-use-pipeline.mjs",
            "deployment": env.get("STAGING_USE_DEPLOYMENT"), "sha": env.get("STAGING_USE_SHA"),
            "expires_at": env.get("STAGING_USE_EXPIRES_AT"),
            "run": env.get("GITHUB_RUN_ID"), "attempt": env.get("GITHUB_RUN_ATTEMPT"),
            "receipt": env.get("STAGING_USE_RECEIPT"), "keys": sorted(env)})
        world.events.append(("lifecycle", argv[2], env.get("STAGING_USE_DEPLOYMENT")))
        outcome = world.refuse_lifecycle.get(index)
        if outcome == "unavailable":
            raise OSError("node unavailable")
        if outcome == "refused":
            return types.SimpleNamespace(returncode=1)
        if argv[2] == "acquire" and world.phase == "active":
            world.running = True  # a proved grant starts an idle singleton
        return types.SimpleNamespace(returncode=0)

    return types.SimpleNamespace(run=run, DEVNULL=subprocess.DEVNULL,
                                 SubprocessError=subprocess.SubprocessError)


def environ(**changes):
    env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": TOKEN,
           "GITHUB_RUN_ID": "123456", "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
           "CLOUDFLARE_API_TOKEN": TOKEN, "GITHUB_RUN_ATTEMPT": ATTEMPT,
           "STAGING_USE_RECEIPT": RECEIPT}
    env.update({key: value for key, value in changes.items() if value is not None})
    for key in [key for key, value in changes.items() if value is None]:
        env.pop(key, None)
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
                world.events.append(("deploy", image, None))
                if image == world.jump_on_deploy:
                    world.tick(WINDOW_MS / 1000)
                if image in world.refuse_deploy:
                    return 1
                # A full `wrangler deploy` creates a NEW version UUID whose
                # immutable metadata is what the generated config bound.
                version = NEW_WORKER.format(len(world.new_versions) + 1)
                meta = {"sha": message_sha,
                        "vars": dict(config["env"]["staging"].get("vars", {}))}
                if world.corrupt_new:
                    world.corrupt_new(world, version, meta)
                    world.corrupt_new = None
                world.metadata[version] = meta
                world.namespaces.setdefault(version, "ns1")
                world.new_versions.append(version)
                world.history.insert(0, version)
                world.serving_image = image
                world.rollouts.append(rollout_row(world, f"r-{len(world.rollouts)}", image))
                return 0
            return deploy

        def make_staging_use(use_env, use_url, use_token):
            return staging_drill_use.StagingDrillUse(
                use_env, use_url, use_token, now=lambda: world.t)

        out, err = io.StringIO(), io.StringIO()
        with patch("builtins.print",
                   lambda *a, **k: out.write(" ".join(map(str, a)) + "\n")), \
                patch.object(container_drill, "POLL_SECONDS", 0), \
                patch.object(staging_drill_use, "subprocess", lifecycle_transport(world)), \
                patch.object(subprocess, "Popen", side_effect=AssertionError("no child")), \
                contextlib.redirect_stderr(err):
            try:
                code = container_drill.main(
                    argv, environ=env,
                    make_control=lambda token, url, actor: make_control(world),
                    client_factory=lambda account, token: FakeClient(world),
                    deploy_factory=deploy_factory,
                    make_staging_use=make_staging_use)
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
    test.assertEqual(world.lifecycle, [],
                     f"dry-run must never acquire, check or release staging use ({label})")


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
                 "invalid_run_id", "run id"),
                (environ(GITHUB_RUN_ATTEMPT=None), "staging_drill_run_invalid",
                 "missing run attempt"),
                (environ(GITHUB_RUN_ATTEMPT="0"), "staging_drill_run_invalid",
                 "zero run attempt"),
                (environ(GITHUB_RUN_ATTEMPT="1; id"), "staging_drill_run_invalid",
                 "injected run attempt"),
                (environ(STAGING_USE_RECEIPT=None), "staging_drill_receipt_missing",
                 "missing receipt"),
                (environ(STAGING_USE_RECEIPT=""), "staging_drill_receipt_missing",
                 "empty receipt")]:
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


def actions(world):
    return [entry["action"] for entry in world.lifecycle]


def live(world, raw_pin=None, env=None):
    code, out, _err, evidence, world = invoke_with_pin(raw_pin or pin(), env or environ(), world)
    return code, out, (json.loads(evidence) if evidence else None), world


class FiniteUseLiveTests(unittest.TestCase):
    """One bounded grant per run/attempt, carried across NEW deploy versions."""

    def test_live_drill_rebinds_one_grant_to_each_new_version(self):
        world = World()
        code, out, evidence, world = live(world)
        self.assertEqual(code, 0, out)
        self.assertEqual(evidence["failures"], [])
        drill_worker, restore_worker = world.new_versions
        self.assertNotIn(drill_worker, (PRE, BACKOUT_WORKER),
                         "a full deploy must answer with a NEW version UUID")
        takeovers = [call[3] for call in world.calls if call[0] == "deployment-takeover"]
        self.assertEqual(takeovers, [drill_worker, restore_worker],
                         "takeover must pin the newly proved version, never the historical pin")
        self.assertNotIn(BACKOUT_WORKER, takeovers)
        self.assertNotIn(PRE, takeovers)
        self.assertEqual(evidence["drill"]["serving_worker_version"], drill_worker)
        self.assertEqual(evidence["drill"]["worker_version"], BACKOUT_WORKER)
        self.assertEqual(evidence["restore"]["serving_worker_version"], restore_worker)
        # Generated config bound the finite policy and compiled SHA explicitly.
        self.assertEqual(world.metadata[drill_worker]["vars"],
                         dict(POLICY, CONTAINER_RELEASE=BACKOUT_SHA))
        self.assertEqual(world.metadata[restore_worker]["vars"],
                         dict(POLICY, CONTAINER_RELEASE=PRE_SHA))
        self.assertEqual(actions(world),
                         ["acquire", "ready", "restore-check", "acquire", "acquire", "release"])
        acquires = [(e["deployment"], e["sha"])
                    for e in world.lifecycle if e["action"] == "acquire"]
        self.assertEqual(acquires, [(PRE, PRE_SHA), (drill_worker, BACKOUT_SHA),
                                    (restore_worker, PRE_SHA)])
        self.assertEqual(world.lifecycle[-1]["deployment"], restore_worker,
                         "release must name the last acquired version")
        # Rebind follows takeover of the same new version (new epoch).
        for version in (drill_worker, restore_worker):
            takeover = world.events.index(("control", "deployment-takeover", version))
            self.assertEqual(world.events[takeover + 1], ("lifecycle", "acquire", version))
        # Exactly one grant: every call carries the original run/attempt/receipt/expiry.
        expiries = {e["expires_at"] for e in world.lifecycle}
        self.assertEqual(len(expiries), 1, "rebind must never extend the grant")
        expiry = int(expiries.pop())
        self.assertEqual(expiry, int(world.lifecycle[0]["t"] * 1000) + WINDOW_MS,
                         "the grant is exactly 25 minutes from acquisition")
        self.assertEqual(evidence["staging_expires_at"], expiry)
        self.assertEqual({(e["run"], e["attempt"], e["receipt"]) for e in world.lifecycle},
                         {("123456", ATTEMPT, RECEIPT)})
        self.assertEqual(evidence["staging_cleanup"], "release_confirmed")
        self.assertLess(world.t * 1000, expiry, "fixture must finish inside the window")
        first_fence = world.events.index(("control", "fence", None))
        self.assertEqual([e for e in world.events[:first_fence] if e[0] == "lifecycle"],
                         [("lifecycle", "acquire", PRE), ("lifecycle", "ready", PRE),
                          ("lifecycle", "restore-check", None)],
                         "one grant is proved before the first mutation")
        for entry in world.lifecycle:
            self.assertNotIn("CLOUDFLARE_API_TOKEN", entry["keys"],
                             "the lifecycle child must not inherit Cloudflare credentials")

    def test_receipt_never_reaches_output_or_evidence(self):
        code, out, _err, evidence, _world = invoke_with_pin(pin(), environ(), World())
        self.assertEqual(code, 0)
        self.assertNotIn(RECEIPT, out)
        self.assertNotIn(RECEIPT, evidence)
        self.assertNotIn(TOKEN, out + evidence)

    def test_idle_baseline_starts_only_after_policy_proof(self):
        code, out, evidence, world = live(World(idle=True))
        self.assertEqual(code, 0, out)
        self.assertEqual(evidence["failures"], [])
        self.assertEqual(actions(world)[:2], ["acquire", "ready"])
        self.assertEqual(world.lifecycle[0]["deployment"], PRE)

    def test_dry_run_refuses_idle_baseline_without_waking_it(self):
        code, out, err, _evidence, world = invoke_with_pin(
            pin(), environ(), World(idle=True), extra=("--dry-run",))
        self.assertEqual(code, 1)
        self.assertIn("singleton_not_active_and_running", out + err)
        assert_zero_mutations(self, world, "idle baseline")
        self.assertFalse(world.running, "a plan must never start an idle baseline")

    def test_dry_run_plan_names_finite_window_and_new_version_proof(self):
        code, out, _err, _evidence, world = invoke_with_pin(
            pin(), environ(), World(), extra=("--dry-run",))
        self.assertEqual(code, 0)
        self.assertIn("25-minute", out)
        self.assertIn("mutations: none", out)
        assert_zero_mutations(self, world, "valid plan")


class FiniteUseHistoricalProofTests(unittest.TestCase):
    """Baseline and historical backout metadata refuse before acquisition."""

    def cases(self):
        def unbounded_backout(world):
            world.metadata[BACKOUT_WORKER]["vars"].pop("STAGING_RUNTIME_POLICY")

        def always_on_backout(world):
            world.metadata[BACKOUT_WORKER]["vars"]["STAGING_RUNTIME_POLICY"] = "always_on"

        def production_baseline(world):
            world.metadata[PRE]["vars"]["TWO_DEPLOY_ENV"] = "production"

        def secret_typed_release(world):
            world.metadata[BACKOUT_WORKER]["vars"]["CONTAINER_RELEASE"] = None

        def other_backout_sha(world):
            world.metadata[BACKOUT_WORKER] = {
                "sha": "c" * 40, "vars": dict(POLICY, CONTAINER_RELEASE="c" * 40)}

        def drifted_identity(world):
            world.version_ids[BACKOUT_WORKER] = PRE

        def unstamped_baseline(world):
            world.metadata[PRE]["sha"] = None

        return [(unbounded_backout, "staging_restore_policy_unproved"),
                (always_on_backout, "staging_restore_policy_unproved"),
                (production_baseline, "staging_restore_policy_unproved"),
                (secret_typed_release, "staging_restore_policy_unproved"),
                (other_backout_sha, "staging_drill_image_mismatch"),
                (drifted_identity, "version_identity_drift"),
                (unstamped_baseline, "staging_restore_identity_invalid")]

    def test_policy_and_sha_metadata_refuse_before_acquisition_in_both_modes(self):
        for corrupt, code in self.cases():
            for extra in ((), ("--dry-run",)):
                with self.subTest(case=corrupt.__name__, mode=extra):
                    world = World()
                    corrupt(world)
                    rc, out, err, _evidence, world = invoke_with_pin(
                        pin(), environ(), world, extra=extra)
                    self.assertEqual(rc, 1)
                    self.assertIn(code, out + err)
                    self.assertEqual(world.lifecycle, [], "refuse before acquiring a grant")
                    self.assertEqual(world.deploys, [])
                    self.assertEqual(mutating_calls(world), [])
                    self.assertTrue(world.running, "an idle-start never happens")

    def test_baseline_serving_other_revision_than_its_metadata_refuses(self):
        world = World()
        world.metadata[PRE] = {"sha": "c" * 40, "vars": dict(POLICY, CONTAINER_RELEASE="c" * 40)}
        code, out, evidence, world = live(world)
        self.assertEqual(code, 1)
        self.assertEqual(evidence["failures"], ["drill:staging_drill_image_mismatch"])
        self.assertEqual(world.deploys, [])
        self.assertEqual(mutating_calls(world), [])
        self.assertEqual(actions(world), ["acquire", "ready", "release"],
                         "an acquired grant is released when preflight refuses")
        self.assertEqual(evidence["staging_cleanup"], "release_confirmed")


class FiniteUseNewVersionProofTests(unittest.TestCase):
    """Each new deploy version is proved before takeover; mismatch stops all."""

    def corruptions(self):
        def drop_policy(world, version, meta):
            meta["vars"].pop("STAGING_RUNTIME_POLICY")

        def wrong_env(world, version, meta):
            meta["vars"]["TWO_DEPLOY_ENV"] = "production"

        def wrong_release(world, version, meta):
            meta["vars"]["CONTAINER_RELEASE"] = PRE_SHA

        def other_source(world, version, meta):
            meta["sha"] = "c" * 40
            meta["vars"]["CONTAINER_RELEASE"] = "c" * 40

        def other_namespace(world, version, meta):
            world.namespaces[version] = "ns2"

        def identity_drift(world, version, meta):
            world.version_ids[version] = BACKOUT_WORKER

        return [(drop_policy, "staging_restore_policy_unproved"),
                (wrong_env, "staging_restore_policy_unproved"),
                (wrong_release, "staging_restore_policy_unproved"),
                (other_source, "staging_drill_image_mismatch"),
                (other_namespace, "incompatible_worker_binding"),
                (identity_drift, "version_identity_drift")]

    def test_new_version_mismatch_refuses_before_takeover_and_stops(self):
        for corrupt, code in self.corruptions():
            with self.subTest(case=corrupt.__name__):
                world = World()
                world.corrupt_new = corrupt
                rc, out, evidence, world = live(world)
                self.assertEqual(rc, 1)
                self.assertEqual(evidence["failures"],
                                 [f"drill:{code}", "restore_skipped_after_control_refusal"])
                self.assertEqual(world.deploys, [BACKOUT_IMAGE], "no restore deploy")
                self.assertEqual([c[0] for c in mutating_calls(world)], ["fence"],
                                 "no takeover of an unproved version")
                self.assertEqual(world.events[-1], ("deploy", BACKOUT_IMAGE, None),
                                 "no control or lifecycle call after the refusal")
                self.assertEqual(actions(world), ["acquire", "ready", "restore-check"],
                                 "no rebind and no release: native expiry owns teardown")
                self.assertEqual(evidence["staging_cleanup"],
                                 "native_expiry_after_control_refusal")
                self.assertNotIn(RECEIPT, out)


class FiniteUseDeadlineTests(unittest.TestCase):
    """Readiness and confirmation stay inside the original deadline."""

    def drill_probe_span(self):
        """First and last readiness probe of the drill leg in an unexpired run."""
        baseline = World()
        fence_probes = []
        original = make_control

        def counting_control(world):
            control = original(world)

            def wrapped(action, *args, **kwargs):
                if action == "fence":
                    fence_probes.append(world.probes)
                return control(action, *args, **kwargs)
            return wrapped

        with patch(f"{__name__}.make_control", counting_control):
            code, _out, _evidence, _ = live(baseline)
        self.assertEqual(code, 0)
        first_drill_probe, last_drill_probe = fence_probes[0] + 1, fence_probes[1]
        self.assertGreater(last_drill_probe, first_drill_probe,
                           "fixture must reach readiness and confirmation probes")
        return first_drill_probe, last_drill_probe

    def test_expiry_during_drill_readiness_or_confirmation_stops_mutation(self):
        first_drill_probe, last_drill_probe = self.drill_probe_span()
        for jump in (first_drill_probe, last_drill_probe):
            with self.subTest(probe=jump):
                world = World()
                world.jump_at_probe = jump
                rc, out, evidence, world = live(world)
                self.assertEqual(rc, 1, out)
                self.assertEqual(evidence["failures"],
                                 ["drill:staging_drill_window_ended",
                                  "restore:staging_drill_window_ended"])
                self.assertIsNotNone(world.jumped)
                after = world.events[world.jumped[0]:]
                self.assertEqual([e for e in after if e[0] in ("control", "deploy")
                                  and e[1] != "status"], [],
                                 "no fence, deploy or takeover after the deadline")
                self.assertEqual([e[1] for e in after if e[0] == "lifecycle"], ["release"],
                                 "only cleanup may follow the deadline")
                self.assertNotIn("restore", evidence, "the restore leg never started")
                self.assertNotIn("first_ready", evidence["drill"],
                                 "readiness confirmed after the deadline is not readiness")
                self.assertEqual(world.probes, jump, "no probe after the deadline")
                self.assertEqual(len({e["expires_at"] for e in world.lifecycle}), 1)
                self.assertEqual(evidence["staging_cleanup"], "release_confirmed")

    def test_expiry_after_confirmation_refuses_cleanly_with_summary(self):
        # RED on 2a5714202: the leg already holds time_to_ready_seconds but
        # not budget_met when the post-confirmation check refuses, so
        # summary() raises KeyError instead of reporting the refusal.
        _first, last_drill_probe = self.drill_probe_span()
        world = World()
        world.jump_on_app_read_after_probe = last_drill_probe
        try:
            rc, out, _err, evidence, world = invoke_with_pin(pin(), environ(), world)
        except KeyError as error:
            self.fail(f"refusal must reach evidence and summary, not crash ({error!r})")
        self.assertEqual(rc, 1, out)
        self.assertIsNotNone(world.jumped)
        evidence = json.loads(evidence)
        self.assertEqual(evidence["failures"],
                         ["drill:staging_drill_window_ended",
                          "restore:staging_drill_window_ended"])
        self.assertEqual(evidence["staging_cleanup"], "release_confirmed")
        self.assertEqual(len(mutating_calls(world)), 2, "drill fence and takeover only")

    def test_expiry_during_deploy_refuses_takeover(self):
        world = World()
        world.jump_on_deploy = BACKOUT_IMAGE
        rc, out, evidence, world = live(world)
        self.assertEqual(rc, 1, out)
        self.assertEqual(evidence["failures"],
                         ["drill:staging_drill_window_ended",
                          "restore:staging_drill_window_ended"])
        self.assertEqual([c[0] for c in mutating_calls(world)], ["fence"],
                         "no takeover once the grant has ended")
        self.assertEqual(world.deploys, [BACKOUT_IMAGE])
        self.assertEqual(actions(world), ["acquire", "ready", "restore-check", "release"])


class FiniteUseRecoveryTests(unittest.TestCase):
    """Fenced partial restore, lifecycle refusal and release failure."""

    def test_fenced_partial_restore_keeps_grant_without_owner_assertion(self):
        world = World()
        world.refuse_deploy.add(BACKOUT_IMAGE)
        code, out, evidence, world = live(world)
        self.assertEqual(code, 1, out)
        self.assertEqual(evidence["failures"], ["drill:deploy_failed"])
        self.assertEqual(world.new_versions, [], "the refused deploy created no version")
        fence = world.events.index(("control", "fence", None))
        self.assertNotIn("restore-check", [e[1] for e in world.events[fence:]],
                         "a fenced recovery never asserts an active owner")
        self.assertEqual([c[0] for c in mutating_calls(world)], ["fence", "deployment-takeover"],
                         "restore takes the fenced singleton over without a second fence")
        self.assertEqual(mutating_calls(world)[-1][3], PRE)
        self.assertEqual(actions(world),
                         ["acquire", "ready", "restore-check", "acquire", "release"])
        self.assertEqual(world.lifecycle[3]["deployment"], PRE)
        self.assertEqual(len({e["expires_at"] for e in world.lifecycle}), 1,
                         "recovery retains the original deadline")
        self.assertEqual(evidence["restore"]["serving_worker_version"], PRE)
        self.assertEqual(evidence["staging_cleanup"], "release_confirmed")

    def test_lifecycle_refusal_stops_later_controls_and_restore(self):
        # Index into ["acquire", "ready", "restore-check", "acquire", ...].
        for index, label, fences, takeovers in (
                (0, "initial acquire", 0, 0), (1, "baseline ready", 0, 0),
                (2, "active-owner check", 0, 0), (3, "rebind", 1, 1)):
            for outcome, code in (("refused", "staging_drill_control_failed"),
                                  ("unavailable", "staging_drill_control_unavailable")):
                with self.subTest(step=label, outcome=outcome):
                    world = World()
                    world.refuse_lifecycle[index] = outcome
                    rc, out, evidence, world = live(world)
                    self.assertEqual(rc, 1, out)
                    expected = [f"drill:{code}"]
                    if fences:
                        expected.append("restore_skipped_after_control_refusal")
                    self.assertEqual(evidence["failures"], expected)
                    self.assertEqual(len(world.lifecycle), index + 1,
                                     "no lifecycle call after a refusal, release included")
                    self.assertEqual(world.events[-1][0], "lifecycle",
                                     "no control or deploy after a refusal")
                    calls = [c[0] for c in mutating_calls(world)]
                    self.assertEqual(calls.count("fence"), fences)
                    self.assertEqual(calls.count("deployment-takeover"), takeovers)
                    self.assertEqual(evidence["staging_cleanup"],
                                     "native_expiry_after_control_refusal")

    def test_release_failure_reports_unconfirmed_native_expiry(self):
        for outcome, code in (("refused", "staging_drill_control_failed"),
                              ("unavailable", "staging_drill_control_unavailable")):
            with self.subTest(outcome=outcome):
                world = World()
                world.refuse_lifecycle[5] = outcome
                rc, out, evidence, world = live(world)
                self.assertEqual(rc, 1, out)
                self.assertEqual(actions(world)[-1], "release")
                self.assertEqual(evidence["failures"], [f"cleanup:{code}"])
                self.assertEqual(evidence["staging_cleanup"], "native_expiry_release_unconfirmed")
                self.assertEqual(evidence["restore"]["running_image"], PRE_IMAGE)


if __name__ == "__main__":
    unittest.main()

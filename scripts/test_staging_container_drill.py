"""Offline staging container backout/restore drill fixtures; stdlib only.

No Cloudflare, no ownership service, no wrangler, no waits: the World fake
owns Worker deployments, the container application, rollout records,
readiness probes and the deploy subprocess.
"""

import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import MagicMock, patch
from urllib.error import HTTPError

spec = importlib.util.spec_from_file_location(
    "staging_container_drill", Path(__file__).with_name("staging_container_drill.py"))
container_drill = importlib.util.module_from_spec(spec)
spec.loader.exec_module(container_drill)
GateError = container_drill.GateError

PRE = "11111111-1111-4111-8111-111111111111"
BACKOUT_WORKER = "22222222-2222-4222-8222-222222222222"
NEVER = "44444444-4444-4444-8444-444444444444"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
ACCOUNT = "b" * 32
SENTINEL = "RAW_SECRET_SENTINEL_never_disclose_ownership_token"
APP = "two-bot-next-twobotcontainer-staging"
REPO = "f" * 16
PRE_IMAGE = f"registry.cloudflare.com/{REPO}/{APP}@sha256:{'a' * 64}"
BACKOUT_IMAGE = f"registry.cloudflare.com/{REPO}/{APP}@sha256:{'b' * 64}"
OTHER_IMAGE = f"registry.cloudflare.com/{REPO}/{APP}@sha256:{'c' * 64}"
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


def pin(**changes):
    base = {"source_sha": BACKOUT_SHA, "build_id": BACKOUT_BUILD,
            "image": BACKOUT_IMAGE, "worker_version": BACKOUT_WORKER,
            "rollout_id": "r-backout", "review_ref": "PR-612:ci-ok-9ed49294",
            "staging_run_id": "37243860909",
            "compatibility_note": "schema-0417+DO-unchanged+flags-unchanged"}
    base.update(changes)
    return base


class World:
    """One staging singleton: Worker versions, app, rollouts, probes, deploy."""

    def __init__(self, history=(PRE, BACKOUT_WORKER), phase="active", epoch=7,
                 ready_polls=1):
        self.history = list(history)  # newest first
        self.phase, self.epoch, self.running = phase, epoch, phase == "active"
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
             "current_version": 39, "current_configuration": {"image": BACKOUT_IMAGE},
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
        self.never_converge = False
        self.refuse_deploy = False
        self.t = 1_800_000_000.0
        self.calls = []
        self.deploys = []
        self.fail_control = {}
        self.raise_on_api = {}  # path prefix -> error to raise

    def tick(self, seconds):
        self.t += seconds

    def revision_for(self, image):
        if image == BACKOUT_IMAGE:
            return BACKOUT_SHA, BACKOUT_BUILD
        if image == PRE_IMAGE:
            return PRE_SHA, PRE_BUILD
        return "unknown", "unknown"


COMPLETED_COUNTS = {"active": 1, "healthy": 1, "failed": 0, "starting": 0,
                    "scheduling": 0}


def rollout_row(world, rid, image, status="completed"):
    # Live rollouts carry distinct creation times; the newest-completed tie
    # below would otherwise resolve to the wrong row.
    created = world.t if isinstance(world, World) else 1_800_000_100.0
    return {"id": rid, "created_at": container_drill.iso(created), "status": status,
            "target_configuration": {"image": image}, "target_version": 42,
            "strategy": "rolling", "kind": "full_auto",
            "current_version": 41, "current_configuration": {"image": image},
            "health": {"instances": dict(COMPLETED_COUNTS)},
            "progress": {"total_steps": 1, "current_step": 1,
                         "total_instances": 1, "updated_instances": 1},
            "steps": [{"status": "completed"}]}


class FakeClient:
    def __init__(self, world):
        self.world = world

    def api(self, path):
        world = self.world
        for prefix, error in world.raise_on_api.items():
            if path.startswith(prefix):
                raise error
        if path == DEPLOYMENTS_PATH:
            rows = []
            for version in world.history:
                versions = [{"version_id": version, "percentage": 100}] if version else \
                    [{"version_id": PRE, "percentage": 50},
                     {"version_id": BACKOUT_WORKER, "percentage": 50}]
                rows.append({"id": "d", "versions": versions})
            return {"deployments": rows}
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
            row = dict(matches[0])
            if world.never_converge:
                row = dict(row, status="progressing")
            return row
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
        # Readiness follows the serving image (the Rust build reports its own
        # revision), not the Worker version id, so one leg can stay dark.
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
        headers = {"x-two-worker-version": version}
        return (200 if up else 503), headers, body.encode()


def make_control(world):
    def control(action, epoch=None, release_fence=False):
        world.tick(1)
        count = sum(1 for call in world.calls if call[0] == action) + 1
        world.calls.append((action, epoch, release_fence))
        failure = world.fail_control.get((action, count))
        if failure:
            raise failure
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


WRANGLER_TOML = """\
name = "two-bot-next"
main = "src/index.ts"

[env.staging.version_metadata]
binding = "CF_VERSION_METADATA"

[[env.staging.containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile"
max_instances = 1
"""


def run_drill(world, raw_pin=None, attestation=COVERING_ATTESTATION, logs=None):
    logs = [] if logs is None else logs
    directory = tempfile.mkdtemp()
    wrangler_dir = Path(directory) / "wrangler"
    wrangler_dir.mkdir()
    (wrangler_dir / "wrangler.toml").write_text(WRANGLER_TOML)
    deploy_path = str(Path(directory) / "staging-container-deploy.json")

    def deploy(config_path):
        config = json.loads(Path(config_path).read_text())
        image = config["env"]["staging"]["containers"][0]["image"]
        world.tick(2)
        world.deploys.append(image)
        if world.refuse_deploy:
            return 1
        world.serving_image = image
        world.rollouts.append(rollout_row(world, f"r-{len(world.rollouts)}", image))
        return 0

    instance = container_drill.Drill(
        FakeClient(world), make_control(world), URL, "123456",
        (deploy_path, deploy, str(wrangler_dir)),
        now=lambda: world.t, sleep=world.tick, log=logs.append)
    validated = container_drill.validate_pin(raw_pin or pin())
    failures = instance.run(validated, container_drill.validate_attestation(attestation))
    return instance, failures, logs


class PinValidationTests(unittest.TestCase):
    def test_valid_pin_accepted(self):
        self.assertEqual(container_drill.validate_pin(pin())["image"], BACKOUT_IMAGE)

    def test_mutable_and_malformed_pins_refused(self):
        cases = [
            ({"source_sha": "latest"}, "invalid_source_sha"),
            ({"source_sha": "9ed49294"}, "invalid_source_sha"),  # short prefix, not a pin
            ({"build_id": "latest"}, "invalid_build_id"),
            ({"image": "registry.cloudflare.com/x/two-bot-next-twobotcontainer-staging:latest"},
             "invalid_image_digest"),
            ({"image": OTHER_IMAGE.replace("@sha256:", ":")}, "invalid_image_digest"),
            ({"worker_version": "latest"}, "invalid_worker_version"),
            ({"rollout_id": "r backout"}, "invalid_rollout_id"),
            ({"review_ref": "see body for review"}, "invalid_review_ref"),
            ({"staging_run_id": "latest"}, "invalid_run_id"),
            ({"compatibility_note": "schema changed; see doc"}, "invalid_compatibility_note"),
        ]
        for change, code in cases:
            with self.subTest(change=change):
                with self.assertRaises(GateError) as caught:
                    container_drill.validate_pin(pin(**change))
                self.assertEqual(str(caught.exception), code)


class AttestationValidationTests(unittest.TestCase):
    def test_absent_attestation_is_none(self):
        self.assertIsNone(container_drill.validate_attestation(""))
        self.assertIsNone(container_drill.validate_attestation(None))

    def test_malformed_attestation_rejected(self):
        for text in ["not json", "[1,2]",
                     json.dumps({"drill": {"distinct_sessions": "one"}}),
                     json.dumps({"drill": {"distinct_sessions": 1,
                                           "window_start": "tomorrow",
                                           "window_end": "never"}}),
                     json.dumps({"drill": {"distinct_sessions": 1,
                                           "window_start": "2026-10-05T00:10:00Z",
                                           "window_end": "2026-10-05T00:00:00Z"}}),
                     json.dumps({"session_id": "abc", "drill": {
                         "distinct_sessions": 1,
                         "window_start": "2026-10-05T00:00:00Z",
                         "window_end": "2026-10-05T00:10:00Z"}})]:
            with self.subTest(text=text[:40]):
                with self.assertRaises(GateError) as caught:
                    container_drill.validate_attestation(text)
                self.assertEqual(str(caught.exception), "invalid_session_attestation")

    def test_identifier_smuggling_rejected(self):
        # Counts and windows only: a session identifier has no valid field.
        text = json.dumps({"drill": {"distinct_sessions": 1,
                                     "window_start": "2026-10-05T00:00:00Z",
                                     "window_end": "2026-10-05T00:10:00Z",
                                     "session_id": SENTINEL}})
        with self.assertRaises(GateError):
            container_drill.validate_attestation(text)


class HappyPathTests(unittest.TestCase):
    def test_backout_then_restore_with_witnessed_sessions(self):
        world = World()
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, [])
        # Both legs deployed through the full-container path with different images.
        self.assertEqual(world.deploys, [BACKOUT_IMAGE, PRE_IMAGE])
        drill, restore = instance.evidence["drill"], instance.evidence["restore"]
        self.assertEqual(drill["intended_image"], BACKOUT_IMAGE)
        self.assertEqual(drill["running_image"], BACKOUT_IMAGE)
        self.assertEqual(restore["running_image"], PRE_IMAGE)
        self.assertNotEqual(drill["running_image"], restore["running_image"])
        for leg in (drill, restore):
            self.assertEqual(leg["session_witness"]["status"], "SINGLE_WITNESSED")
            self.assertTrue(leg["budget_met"])
            self.assertLessEqual(leg["time_to_ready_seconds"], 60)
            self.assertGreaterEqual(leg["outage_seconds"], leg["time_to_ready_seconds"])
        self.assertEqual(instance.evidence["pre_image"], PRE_IMAGE)
        self.assertEqual(instance.evidence["backout"]["source_sha"], BACKOUT_SHA)
        self.assertEqual(world.serving_image, PRE_IMAGE)
        self.assertEqual((world.phase, world.running), ("active", True))

    def test_fence_takeover_order_and_epochs(self):
        world = World()
        run_drill(world)
        actions = [call[0] for call in world.calls]
        self.assertEqual(actions,
                         ["status", "status", "fence", "deployment-takeover",
                          "status", "fence", "deployment-takeover"])
        self.assertTrue(all(call[2] for call in world.calls
                            if call[0] == "deployment-takeover"))

    def test_deploy_config_overrides_only_the_image(self):
        world = World()
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, [])
        self.assertNotIn("../Dockerfile", Path(instance.deploy_path).read_text())
        config = json.loads(Path(instance.deploy_path).read_text())
        containers = config["env"]["staging"]["containers"]
        self.assertEqual(len(containers), 1)
        self.assertEqual(containers[0]["image"], PRE_IMAGE)  # last leg wrote restore
        self.assertEqual(containers[0]["rollout_kind"], "full_auto")
        self.assertNotIn("image_vars", containers[0])
        self.assertEqual(config["name"], "two-bot-next")
        self.assertTrue(os.path.isabs(config["main"]))


class RefusalTests(unittest.TestCase):
    def assert_refused_without_change(self, world, raw_pin, code):
        instance, failures, _ = run_drill(world, raw_pin)
        self.assertEqual(failures, [f"drill:{code}"])
        self.assertEqual(world.deploys, [])
        self.assertNotIn("fence", [call[0] for call in world.calls])
        self.assertFalse(instance.mutated)

    def test_same_image_as_serving_refused(self):
        self.assert_refused_without_change(World(), pin(image=PRE_IMAGE),
                                           "same_image_as_serving")

    def test_backout_worker_must_have_served_traffic(self):
        self.assert_refused_without_change(World(), pin(worker_version=NEVER),
                                           "backout_worker_never_served")

    def test_backout_rollout_absent_refused(self):
        world = World()
        world.rollouts = [row for row in world.rollouts if row["id"] != "r-backout"]
        self.assert_refused_without_change(world, pin(), "backout_rollout_absent")

    def test_backout_rollout_not_completed_refused(self):
        # Live run 5: pin present with a matching image but status `replaced`
        # after 5 newer rollouts. Strict policy keeps it unproven.
        for status in ("replaced", "reverted", "progressing", "pending"):
            with self.subTest(status=status):
                world = World()
                for row in world.rollouts:
                    if row["id"] == "r-backout":
                        row["status"] = status
                self.assert_refused_without_change(world, pin(), "backout_rollout_not_completed")

    def test_backout_rollout_image_mismatch_refused(self):
        world = World()
        for row in world.rollouts:
            if row["id"] == "r-backout":
                row["target_configuration"] = {"image": OTHER_IMAGE}
        self.assert_refused_without_change(world, pin(), "backout_rollout_image_mismatch")

    def test_incompatible_worker_binding_refused(self):
        world = World()
        world.namespaces[BACKOUT_WORKER] = "ns-other"
        self.assert_refused_without_change(world, pin(), "incompatible_worker_binding")

    def test_singleton_must_be_active_and_running(self):
        self.assert_refused_without_change(World(phase="fenced"), pin(),
                                           "singleton_not_active_and_running")

    def test_baseline_must_be_ready_with_proven_revision(self):
        world = World(ready_polls=0)
        world.never_ready.add(PRE_IMAGE)
        self.assert_refused_without_change(world, pin(), "baseline_not_ready")

    def test_split_traffic_is_never_a_baseline(self):
        world = World(history=(None, PRE))
        instance, failures, _ = run_drill(world, pin())
        self.assertEqual(failures, ["drill:split_traffic_deployment"])
        self.assertEqual(world.deploys, [])


class FailureRecoveryTests(unittest.TestCase):
    def test_refused_backout_deploy_still_restores_takeover_only(self):
        world = World()
        world.refuse_deploy = True
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, ["drill:deploy_failed"])
        # One deploy attempt (backout); the restore found the baseline still
        # serving and released the fence without a second deploy.
        self.assertEqual(len(world.deploys), 1)
        self.assertEqual((world.phase, world.running), ("active", True))
        self.assertEqual(instance.evidence["restore"]["running_image"], PRE_IMAGE)

    def test_target_that_never_becomes_ready_is_restored(self):
        world = World()
        world.never_ready.add(BACKOUT_IMAGE)  # the backout image never reports ready
        world.ready_polls = 0
        instance, failures, _ = run_drill(world)
        self.assertEqual([item.split(":")[0] for item in failures], ["drill"])
        self.assertTrue(failures[0].startswith("drill:ready_timeout"))
        self.assertEqual(world.deploys, [BACKOUT_IMAGE, PRE_IMAGE])
        self.assertEqual(world.serving_image, PRE_IMAGE)

    def test_ownership_auth_failure_skips_restore_without_substitution(self):
        world = World()
        world.fail_control[("deployment-takeover", 1)] = GateError("ownership_auth_failed")
        instance, failures, logs = run_drill(world)
        self.assertEqual(failures, ["drill:ownership_auth_failed",
                                   "restore_skipped_after_auth_failure"])
        self.assertEqual(len(world.deploys), 1)  # no rollback of the rollback
        self.assertTrue(any("never substitute credentials" in line for line in logs))

    def test_cloudflare_auth_failure_also_skips_the_restore(self):
        world = World()
        world.fail_control[("deployment-takeover", 1)] = GateError(
            "api_http_failure", "status=403 codes=10000")
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, ["drill:api_http_failure",
                                   "restore_skipped_after_auth_failure"])
        self.assertEqual(len(world.deploys), 1)

    def test_failed_classifies_auth_only_on_401_403(self):
        world = World()
        logs = []
        instance = container_drill.Drill(
            FakeClient(world), make_control(world), URL, "1", ("x", lambda p: 0, "y"),
            now=lambda: world.t, sleep=world.tick, log=logs.append)
        self.assertEqual(instance.failed("drill", GateError(
            "api_http_failure", "status=403 codes=10000")), "drill:api_http_failure")
        self.assertTrue(instance.auth_failed)
        instance.auth_failed = False
        instance.failed("drill", GateError("api_http_failure", "status=409 codes=10220"))
        self.assertFalse(instance.auth_failed)

    def test_unexpected_exception_still_restores_without_a_traceback(self):
        world = World()
        world.fail_control[("deployment-takeover", 1)] = KeyError(SENTINEL)
        instance, failures, logs = run_drill(world)
        self.assertEqual(failures, ["drill:unexpected_error"])
        self.assertNotIn(SENTINEL, "\n".join(logs))
        self.assertEqual(world.serving_image, PRE_IMAGE)


class SessionWitnessTests(unittest.TestCase):
    def test_absent_attestation_is_not_proven_not_pass(self):
        world = World()
        instance, failures, _ = run_drill(world, attestation="")
        self.assertEqual(failures, ["drill:session_not_proven",
                                   "restore:session_not_proven"])
        # The mechanism still ran: different image out, baseline restored.
        self.assertEqual(world.deploys, [BACKOUT_IMAGE, PRE_IMAGE])
        self.assertEqual(instance.evidence["drill"]["session_witness"],
                         {"status": "NOT_PROVEN", "reason": "session_log_count_absent"})

    def test_ambiguous_count_is_not_proven(self):
        world = World()
        attestation = json.dumps({
            "drill": {"distinct_sessions": 2,
                      "window_start": "1970-01-01T00:00:00Z",
                      "window_end": "2286-11-20T17:46:39Z"}})
        _, failures, _ = run_drill(world, attestation=attestation)
        self.assertIn("drill:session_not_proven", failures)

    def test_uncovered_window_is_not_proven(self):
        world = World()
        attestation = json.dumps({
            "drill": {"distinct_sessions": 1,
                      "window_start": "2026-10-05T00:00:00Z",
                      "window_end": "2026-10-05T00:00:01Z"},
            "restore": {"distinct_sessions": 1,
                        "window_start": "2026-10-05T00:00:00Z",
                        "window_end": "2026-10-05T00:00:01Z"}})
        instance, failures, _ = run_drill(world, attestation=attestation)
        self.assertIn("drill:session_not_proven", failures)
        self.assertEqual(instance.evidence["drill"]["session_witness"]["reason"],
                         "attestation_window_uncovered")

    def test_confirmation_disagreement_keeps_polling(self):
        world = World(ready_polls=0)
        logs = []
        client = FakeClient(world)
        states = iter([PRE, BACKOUT_WORKER, PRE])

        original_request = client.request

        def flapping(url):
            status, headers, body = original_request(url)
            if url.endswith("/readyz"):
                headers = dict(headers)
                try:
                    headers["x-two-worker-version"] = next(states)
                except StopIteration:
                    pass
            return status, headers, body

        client.request = flapping
        instance = container_drill.Drill(
            client, make_control(world), URL, "1", ("x", lambda p: 0, "y"),
            now=lambda: world.t, sleep=world.tick, log=logs.append)
        self.assertFalse(instance.confirm(world.t, PRE, PRE_SHA, PRE_BUILD))

    def test_evaluate_session_reasons(self):
        fence, ready = 100.0, 120.0
        covering = {"distinct_sessions": 1, "window_start": 0.0, "window_end": 200.0}
        timeline = [(110.0, 200, True, True, True)]
        self.assertEqual(container_drill.evaluate_session([], covering, fence, ready),
                         {"status": "NOT_PROVEN", "reason": "timeline_empty"})
        self.assertEqual(container_drill.evaluate_session(
            [(110.0, 503, False, False, False)], covering, fence, ready),
            {"status": "NOT_PROVEN", "reason": "no_ready_probe"})
        self.assertEqual(container_drill.evaluate_session(timeline, None, fence, ready),
                         {"status": "NOT_PROVEN", "reason": "session_log_count_absent"})
        self.assertEqual(container_drill.evaluate_session(
            timeline, {**covering, "distinct_sessions": 0}, fence, ready),
            {"status": "NOT_PROVEN", "reason": "session_count_ambiguous"})
        witnessed = container_drill.evaluate_session(timeline, covering, fence, ready)
        self.assertEqual(witnessed["status"], "SINGLE_WITNESSED")


class EvidenceTests(unittest.TestCase):
    def test_success_writes_allowlisted_evidence_and_summary(self):
        world = World()
        instance, failures, logs = run_drill(world)
        text = container_drill.summary(instance.evidence, failures)
        self.assertIn("PASS", text)
        self.assertIn(BACKOUT_IMAGE, text)
        self.assertIn("SINGLE_WITNESSED", text)
        for blob in (json.dumps(instance.evidence), text, "\n".join(logs)):
            self.assertNotIn(SENTINEL, blob)

    def test_api_error_envelope_keeps_only_status_and_integer_codes(self):
        client = container_drill.ContainerDrillClient(ACCOUNT, SENTINEL)
        envelope = json.dumps({"errors": [{"code": 10220, "message": SENTINEL},
                                          {"code": "text", "message": SENTINEL}]}).encode()
        error = HTTPError("https://api.cloudflare.com", 409, "Conflict", {},
                          io.BytesIO(envelope))
        client.opener = MagicMock()
        client.opener.open.side_effect = error
        with self.assertRaises(GateError) as caught:
            client.api("/workers/scripts/two-bot-next-staging/deployments")
        self.assertEqual(str(caught.exception), "api_http_failure")
        self.assertEqual(caught.exception.detail, "status=409 codes=10220")
        self.assertNotIn(SENTINEL, repr(caught.exception.detail))

    def test_api_sends_bearer_to_the_account_path_only(self):
        client = container_drill.ContainerDrillClient(ACCOUNT, SENTINEL)
        response = MagicMock()
        response.__enter__.return_value = response
        response.status = 200
        response.read.return_value = b'{"success": true, "result": {"id": "x"}}'
        client.opener = MagicMock()
        client.opener.open.return_value = response
        self.assertEqual(client.api("/x"), {"id": "x"})
        request = client.opener.open.call_args.args[0]
        self.assertTrue(request.full_url.startswith(
            f"https://api.cloudflare.com/client/v4/accounts/{ACCOUNT}"))
        self.assertEqual(request.get_header("Authorization"), f"Bearer {SENTINEL}")

    def test_unrecognised_owner_phase_is_never_echoed(self):
        world = World()
        control = make_control(world)

        def hostile(action, epoch=None, release_fence=False):
            state = control(action, epoch, release_fence)
            if action == "status" and world.calls.count(("status", None, False)) >= 2:
                state["owner"]["phase"] = f"weird {SENTINEL}"
            return state

        logs = []
        instance = container_drill.Drill(
            FakeClient(world), hostile, URL, "1", ("x", lambda p: 0, "y"),
            now=lambda: world.t, sleep=world.tick, log=logs.append)
        failures = instance.run(container_drill.validate_pin(pin()), None)
        self.assertEqual(failures, ["drill:owner_state_unexpected"])
        self.assertNotIn(SENTINEL, "\n".join(logs))
        self.assertIn("phase=unrecognized", "\n".join(logs))

    def test_failed_leg_records_how_far_it_got(self):
        world = World()
        world.refuse_deploy = True
        instance, failures, _ = run_drill(world)
        leg = instance.evidence["drill"]
        self.assertEqual(set(leg) - {"session_witness"},
                         {"worker_version", "source_sha", "build_id",
                          "intended_image", "start", "fenced"})
        text = container_drill.summary(instance.evidence, failures)
        self.assertIn("incomplete; reached: start, fenced", text)
        self.assertIn("FAIL", text)


class ConfigTests(unittest.TestCase):
    SOURCE = Path(container_drill.__file__).read_text()

    def test_no_old_source_checkout_and_no_rebuild(self):
        lowered = self.SOURCE.lower()
        self.assertNotIn("git checkout", lowered)
        self.assertNotIn("git fetch", lowered)
        self.assertNotIn("docker build", lowered)
        self.assertNotIn("force=true", self.SOURCE)
        self.assertNotIn("--yes", self.SOURCE)

    def test_worker_name_is_the_staging_worker_only(self):
        self.assertEqual(container_drill.WORKER, "two-bot-next-staging")
        self.assertNotIn("production", self.SOURCE.lower().replace("never production", ""))

    def test_wrong_wrangler_config_refused(self):
        directory = tempfile.mkdtemp()
        wrangler_dir = Path(directory) / "wrangler"
        wrangler_dir.mkdir()
        (wrangler_dir / "wrangler.toml").write_text(
            WRANGLER_TOML.replace('name = "two-bot-next"', 'name = "other"'))
        with self.assertRaises(GateError) as caught:
            container_drill.generate_deploy_config(str(Path(directory) / "out.json"),
                                                   BACKOUT_IMAGE, str(wrangler_dir))
        self.assertEqual(str(caught.exception), "wrong_staging_config")


class MainTests(unittest.TestCase):
    def environ(self, **changes):
        env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": SENTINEL,
               "GITHUB_RUN_ID": "123456", "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
               "CLOUDFLARE_API_TOKEN": "t" * 40}
        env.update(changes)
        return env

    def args(self, directory, **changes):
        base = {"backout_source_sha": BACKOUT_SHA, "backout_build_id": BACKOUT_BUILD,
                "backout_image": BACKOUT_IMAGE, "backout_worker_version": BACKOUT_WORKER,
                "backout_rollout_id": "r-backout", "backout_review_ref": "PR-612:ci-ok",
                "backout_staging_run_id": "37243860909",
                "compatibility_note": "schema-0417+DO-unchanged",
                "session_attestation": COVERING_ATTESTATION,
                "evidence": str(Path(directory) / "evidence.json"),
                "deploy_config": str(Path(directory) / "deploy.json")}
        base.update(changes)
        argv = []
        for key, value in base.items():
            argv += [f"--{key.replace('_', '-')}", value]
        return argv

    def invoke(self, env, world=None, argv=None):
        world = world or World()
        with tempfile.TemporaryDirectory() as directory:
            wrangler_dir = Path(directory) / "wrangler"
            wrangler_dir.mkdir()
            (wrangler_dir / "wrangler.toml").write_text(WRANGLER_TOML)
            evidence = Path(directory) / "evidence.json"
            summary = Path(directory) / "summary.md"
            env = {**env, "GITHUB_STEP_SUMMARY": str(summary)}

            def deploy_factory(wrangler_bin, directory):
                def deploy(config_path):
                    config = json.loads(Path(config_path).read_text())
                    world.deploys.append(
                        config["env"]["staging"]["containers"][0]["image"])
                    world.tick(2)
                    world.serving_image = config["env"]["staging"]["containers"][0]["image"]
                    world.rollouts.append(rollout_row(
                        world, f"r-{len(world.rollouts)}", world.serving_image))
                    return 0
                return deploy

            output = io.StringIO()
            with patch("builtins.print",
                       lambda *a, **k: output.write(" ".join(map(str, a)) + "\n")):
                code = container_drill.main(
                    argv or self.args(directory), environ=env,
                    make_control=lambda token, url, actor: make_control(world),
                    client_factory=lambda account, token: FakeClient(world),
                    deploy_factory=deploy_factory)
            return (code, output.getvalue(),
                    evidence.read_text() if evidence.exists() else None,
                    summary.read_text() if summary.exists() else None, world)

    def test_success_writes_evidence_and_summary_without_secrets(self):
        code, output, evidence, summary, _ = self.invoke(self.environ())
        self.assertEqual(code, 0)
        data = json.loads(evidence)
        self.assertEqual(data["failures"], [])
        self.assertIn("PASS", summary)
        for text in (output, evidence, summary):
            self.assertNotIn(SENTINEL, text)

    def test_bad_environment_is_refused_before_any_change(self):
        cases = [
            self.environ(STAGING_WORKER_URL="https://two-bot-next.5150.workers.dev"),
            self.environ(STAGING_WORKER_URL="http://two-bot-next-staging.x.workers.dev"),
            self.environ(OWNERSHIP_CONTROL_TOKEN="short"),
            self.environ(GITHUB_RUN_ID="12 ; rm -rf"),
        ]
        for env in cases:
            with self.subTest(env={k: v for k, v in env.items()
                                   if k != "OWNERSHIP_CONTROL_TOKEN"}):
                world = World()
                with tempfile.TemporaryDirectory() as directory:
                    code, output, evidence, _, _ = self.invoke(env, world,
                                                              self.args(directory))
                self.assertEqual(code, 1)
                self.assertEqual(world.calls, [])
                self.assertEqual(world.deploys, [])
                self.assertIsNone(evidence)
                self.assertNotIn(SENTINEL, output)
                self.assertIn("refused before any change", output)


if __name__ == "__main__":
    unittest.main()

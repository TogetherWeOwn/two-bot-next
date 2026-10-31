"""Offline pins for scripts/worker_container_rollback.py.

No Cloudflare or ownership credentials: the control plane is faked in
memory, time is scripted, and the ownership client is a recording stub.
Nothing here dispatches a workflow or moves traffic.
"""

import dataclasses
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


def good_image():
    return IMAGE_AT + "a" * 64


def bad_image():
    return IMAGE_AT + "b" * 64


def components(gateway="ready"):
    return [["process", "ready"], ["gateway", gateway], ["database", "ready"],
            ["token_invalid", "ready"]]


def rollout_row(rollout_id, image, created="2026-10-15T00:00:00Z", status="completed",
                target_version=7, steps=None, progress=None):
    return {"id": rollout_id, "created_at": created, "status": status,
            "strategy": "rolling", "kind": "full_auto", "target_version": target_version,
            "current_version": target_version - 1,
            "target_configuration": {"image": image, "instance_type": "lite"},
            "current_configuration": {"image": image},
            "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                     "starting": 0, "scheduling": 0}},
            "progress": {"total_steps": 1, "current_step": 1, "total_instances": 1,
                         "updated_instances": 1} if progress is None else progress,
            "steps": [{"status": "completed"}] if steps is None else steps}


def unconverged_row(rollout_id, image, **overrides):
    """A replaced rollout superseded in flight: steps still running, no
    instance updated. Status alone must never prove the image served."""
    overrides.setdefault("status", "replaced")
    overrides.setdefault("steps", [{"status": "in_progress"}])
    overrides.setdefault("progress", {"total_steps": 1, "current_step": 1,
                                      "total_instances": 1, "updated_instances": 0})
    return rollout_row(rollout_id, image, **overrides)


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


def ready_probe(version, revision=SHA, gateway="ready", health=200, status=200):
    def handler():
        body = json.dumps({"components": components(gateway), "jobs": {},
                           "build_revision": revision, "build_id": "1-1"}).encode()
        return status, {"x-two-worker-version": version}, body
    def health_handler():
        return health, {}, b"{}"

    return handler, health_handler


def control_stub(epoch=9):
    calls = []
    phase = {"current": "active"}

    def control(action, epoch_arg=None, expected_deployment=None):
        calls.append((action, epoch_arg, expected_deployment))
        if action == "status":
            return {"owner": {"phase": phase["current"], "epoch": epoch}, "running": True}
        if action == "fence":
            phase["current"] = "fenced"
        else:
            phase["current"] = "active"
        return {"owner": {"phase": phase["current"], "epoch": epoch + 1,
                          "deploymentId": expected_deployment},
                "deploymentId": expected_deployment}
    return control, calls


class Clock:
    def __init__(self):
        self.t = 0.0

    def now(self):
        return self.t

    def sleep(self, seconds):
        self.t += seconds


def make_rollback(routes, variant="staging", control=None, now=None, sleep=None,
                  expected_sha=SHA, clock=None):
    control = control or control_stub()[0]
    log = io.StringIO()
    client = FakeClient(routes)
    clock = clock or Clock()
    rb = rollback.Rollback(
        client, control, "https://two-bot-next-staging.workers.dev", variant,
        WORKER, APPLICATION, APP_ID, expected_sha, "700",
        now=now or clock.now, sleep=sleep or clock.sleep,
        log=lambda msg: log.write(msg + "\n"))
    return rb, client, log


def version_route(version=GOOD_VERSION, message=SHA):
    result = {"id": version}
    if message is not None:
        result["annotations"] = {"workers/message": message}
    return lambda: {"success": True, "result": result}


class SnapshotTests(unittest.TestCase):
    def test_snapshot_records_allowlisted_serving_state(self):
        prev = rollout_row("r-prev", good_image(), created="2026-10-14T00:00:00Z")
        routes = {
            ("GET", "/containers/applications"): lambda: {
                "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                             "durable_objects": {"namespace_id": NAMESPACE}}]},
            ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"): lambda: {
                "success": True, "result": [prev]},
            ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
                "success": True, "result": {"deployments": [
                    {"versions": [{"version_id": GOOD_VERSION, "percentage": 100}]}]}},
            ("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"): version_route(),
        }
        state = rollback.snapshot_state(FakeClient(routes), WORKER, APPLICATION)
        self.assertEqual(state, {"has_previous": True, "worker": WORKER,
                                 "application": APPLICATION, "application_id": APP_ID,
                                 "namespace_id": NAMESPACE, "serving_version": GOOD_VERSION,
                                 "serving_sha": SHA, "provenance_complete": True,
                                 "serving_rollout_id": "r-prev", "serving_image": good_image(),
                                 "recorded_at": state["recorded_at"]})
        # The previous target configuration is re-read at rollback time and
        # held in memory only: it must never reach the snapshot artifact.
        self.assertNotIn("target_configuration", json.dumps(state))

    def test_snapshot_of_pre_stamp_version_records_no_sha(self):
        # A version deployed before --message stamping has no SHA member:
        # the snapshot records the incomplete receipt instead of blocking
        # the deploy it precedes. A non-SHA message is just as absent.
        for message in (None, "deploy from dashboard", "0123abc"):
            with self.subTest(message=message):
                prev = rollout_row("r-prev", good_image(), created="2026-10-14T00:00:00Z")
                routes = {
                    ("GET", "/containers/applications"): lambda: {
                        "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                                     "durable_objects": {"namespace_id": NAMESPACE}}]},
                    ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"): lambda: {
                        "success": True, "result": [prev]},
                    ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
                        "success": True, "result": {"deployments": [
                            {"versions": [{"version_id": GOOD_VERSION, "percentage": 100}]}]}},
                    ("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"):
                        version_route(message=message),
                }
                state = rollback.snapshot_state(FakeClient(routes), WORKER, APPLICATION)
                self.assertTrue(state["has_previous"])
                self.assertIsNone(state["serving_sha"])
                self.assertFalse(state["provenance_complete"])

    def test_version_sha_refuses_identity_drift(self):
        routes = {("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"):
                  version_route(version=BAD_VERSION)}
        with self.assertRaises(base.GateError) as raised:
            rollback.version_sha(FakeClient(routes), WORKER, GOOD_VERSION)
        self.assertEqual(str(raised.exception), "version_identity_drift")

    def test_split_traffic_has_no_previous(self):
        routes = {
            ("GET", "/containers/applications"): lambda: {
                "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                             "durable_objects": {"namespace_id": NAMESPACE}}]},
            ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
                "success": True, "result": {"deployments": [
                    {"versions": [{"version_id": GOOD_VERSION, "percentage": 50},
                                  {"version_id": BAD_VERSION, "percentage": 50}]}]}},
        }
        state = rollback.snapshot_state(FakeClient(routes), WORKER, APPLICATION)
        self.assertEqual(state["has_previous"], False)

    def test_first_deploy_records_no_previous_instead_of_failing(self):
        routes = {
            ("GET", "/containers/applications"): lambda: {
                "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                             "durable_objects": {"namespace_id": NAMESPACE}}]},
            ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
                "success": True, "result": {"deployments": []}},
        }
        state = rollback.build_snapshot(FakeClient(routes), WORKER, APPLICATION)
        self.assertEqual(state["has_previous"], False)

    def test_api_failure_still_fails_closed(self):
        routes = {("GET", "/containers/applications"): lambda: {"success": False}}
        with self.assertRaises(base.GateError):
            rollback.build_snapshot(FakeClient(routes), WORKER, APPLICATION)

    def test_ambiguous_application_fails_closed(self):
        routes = {("GET", "/containers/applications"): lambda: {
            "success": True, "result": []}}
        with self.assertRaises(base.GateError):
            rollback.snapshot_state(FakeClient(routes), WORKER, APPLICATION)


class TargetImageTests(unittest.TestCase):
    def test_accepts_digest_pinned_image_for_the_application(self):
        self.assertEqual(rollback.restore_image_shape(good_image(), APPLICATION),
                         good_image())

    def test_rejects_anything_else(self):
        for bad in ("latest", good_image().replace("@sha256:", ":latest@sha256:"),
                    IMAGE_AT + "z" * 64,
                    good_image().replace(APPLICATION, "other-app"),
                    good_image()[: len(good_image()) - 1],
                    None, 42):
            with self.subTest(bad=str(bad)[:40]):
                with self.assertRaises(base.GateError):
                    rollback.restore_image_shape(bad, APPLICATION)

    def test_rollout_image_validates_per_application(self):
        row = rollout_row("r-1", good_image())
        self.assertEqual(rollback.rollout_image(row, APPLICATION), good_image())
        with self.assertRaises(base.GateError):
            rollback.rollout_image(row, "other-app")
        with self.assertRaises(base.GateError):
            rollback.rollout_image({"target_configuration": {}}, APPLICATION)


class GatewayStateTests(unittest.TestCase):
    def test_gateway_states(self):
        self.assertEqual(rollback.gateway_state({"components": components("ready")}), "ready")
        self.assertEqual(rollback.gateway_state({"components": components("down")}), "down")
        self.assertEqual(rollback.gateway_state({"components": components("bogus")}), "unready")
        self.assertEqual(rollback.gateway_state({"components": []}), "absent")
        self.assertEqual(rollback.gateway_state({}), "absent")
        self.assertEqual(rollback.gateway_state(None), "absent")


class GatePassTests(unittest.TestCase):
    def routes(self, **probe):
        handler, health = ready_probe(**probe)
        return {("PROBE", "readyz"): handler, ("PROBE", "health"): health}

    def test_fully_ready_passes(self):
        rb, _, _ = make_rollback(self.routes(version=BAD_VERSION))
        passed, observation = rb.gate_pass(BAD_VERSION)
        self.assertTrue(passed)
        self.assertIn("gateway=ready", observation)

    def test_parked_gateway_fails(self):
        for kwargs in ({"version": BAD_VERSION, "gateway": "down"},
                       {"version": BAD_VERSION, "gateway": "starting"},
                       {"version": BAD_VERSION, "status": 503},
                       {"version": BAD_VERSION, "health": 503},
                       {"version": GOOD_VERSION},
                       {"version": BAD_VERSION, "revision": "f" * 40}):
            with self.subTest(kwargs=kwargs):
                rb, _, _ = make_rollback(self.routes(**kwargs))
                passed, _ = rb.gate_pass(BAD_VERSION)
                self.assertFalse(passed)

    def test_pre_stamp_version_is_recorded_not_failed(self):
        def unstamped():
            body = json.dumps({"components": components(), "jobs": {},
                               "build_revision": "unknown",
                               "build_id": "unknown"}).encode()
            return 200, {"x-two-worker-version": BAD_VERSION}, body
        rb, _, _ = make_rollback({("PROBE", "readyz"): unstamped,
                                  ("PROBE", "health"): lambda: (200, {}, b"{}")})
        passed, _ = rb.gate_pass(BAD_VERSION)
        self.assertTrue(passed)
        self.assertTrue(rb.evidence.get("pre_stamp_recorded"))

    def test_snapshot_rollback_ignores_the_failed_build_revision(self):
        # Auto-rollback restores the previous build, whose /readyz reports
        # the previous revision instead of the failed SHA. Without
        # --expected-sha the gate trusts the version header plus gateway
        # readiness; with it, the same answer must still fail.
        routes = self.routes(version=BAD_VERSION, revision="f" * 40)
        rb, _, _ = make_rollback(routes, expected_sha=None)
        passed, observation = rb.gate_pass(BAD_VERSION)
        self.assertTrue(passed, observation)
        self.assertIn("gateway=ready", observation)
        rb, _, _ = make_rollback(routes, expected_sha=SHA)
        passed, _ = rb.gate_pass(BAD_VERSION)
        self.assertFalse(passed)

    def test_hostile_body_never_reaches_observation(self):
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        body = json.dumps({"components": [["gateway", "ready\nextra " + marker]],
                           "build_revision": SHA, "build_id": "1-1"}).encode()

        def hostile():
            return 200, {"x-two-worker-version": BAD_VERSION}, body
        rb, _, log = make_rollback({("PROBE", "readyz"): hostile,
                                    ("PROBE", "health"): lambda: (200, {}, b"{}")})
        passed, observation = rb.gate_pass(BAD_VERSION)
        self.assertFalse(passed)
        self.assertNotIn(marker, observation + log.getvalue())


class StableGateTests(unittest.TestCase):
    def test_streak_resets_on_a_miss(self):
        answers = iter([True] * 5 + [False] + [True] * rollback.STABLE_PASSES)
        rb, _, log = make_rollback({})
        rb.gate_pass = lambda version: (next(answers), "readyz=200 gateway=ready")
        rb.wait_stable(BAD_VERSION, 0.0)
        polls = log.getvalue().count("gate poll")
        self.assertEqual(polls, 5 + 1 + rollback.STABLE_PASSES)
        self.assertEqual(rb.evidence["stable_polls"], polls)

    def test_deadline_fails_closed(self):
        rb, _, _ = make_rollback({}, now=lambda: 10 ** 9)
        rb.gate_pass = lambda version: (True, "readyz=200 gateway=ready")
        with self.assertRaises(base.GateError):
            rb.wait_stable(BAD_VERSION, 0.0)


class RollbackRunTests(unittest.TestCase):
    def test_stable_gate_budget_starts_after_restore_steps(self):
        clock = Clock()
        rb, _, _ = make_rollback({}, clock=clock)
        gate_started = []
        rb.restore_image = lambda image: clock.sleep(400)
        rb.take_over = lambda version: clock.sleep(500)
        rb.wait_stable = lambda version, started: gate_started.append(started)
        with mock.patch.object(rollback, "serving_version", return_value=GOOD_VERSION):
            rb.run(GOOD_VERSION, good_image())
        self.assertEqual(gate_started, [900.0])

    def routes(self, serving=BAD_VERSION):
        prev = rollout_row("r-prev", good_image(), created="2026-10-14T00:00:00Z")
        created = {"n": 0}

        def post_rollouts(body):
            created["n"] += 1
            return {"id": "r-new", "target_version": 9}

        def get_rollout():
            # Converged once the new rollout record exists.
            status = "completed" if created["n"] else "progressing"
            row = rollout_row("r-new", good_image(), created="2026-10-15T01:00:00Z",
                              status=status, target_version=9)
            return {"success": True, "result": row}

        def post_deployments(body):
            routes_state["serving"] = body["versions"][0]["version_id"]
            return {"ok": True}

        routes_state = {"serving": serving}
        handler, health = ready_probe(GOOD_VERSION)
        routes = {
            ("GET", f"/workers/scripts/{WORKER}/deployments"):
                lambda: {"success": True, "result": {"deployments": [
                    {"versions": [{"version_id": routes_state["serving"],
                                   "percentage": 100}]}]}},
            ("POST", f"/workers/scripts/{WORKER}/deployments"): post_deployments,
            ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"):
                lambda: {"success": True, "result": [prev]},
            ("POST", f"/containers/applications/{APP_ID}/rollouts"): post_rollouts,
            ("GET", f"/containers/applications/{APP_ID}/rollouts/r-new"): get_rollout,
            ("GET", "/containers/applications"): lambda: {
                "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                             "durable_objects": {"namespace_id": NAMESPACE},
                                             "configuration": {"image": good_image()}}]},
            ("PROBE", "readyz"): handler,
            ("PROBE", "health"): health,
        }
        return routes, routes_state

    def test_full_restore_posts_worker_and_image_then_hands_over(self):
        routes, state = self.routes()
        control, calls = control_stub()
        rb, client, _ = make_rollback(routes, control=control)
        evidence = rb.run(GOOD_VERSION, good_image())
        posts = [call for call in client.calls if call[0] == "POST"]
        worker_post = next(body for method, path, body in posts if "deployments" in path)
        self.assertEqual(worker_post["versions"], [{"version_id": GOOD_VERSION,
                                                    "percentage": 100}])
        self.assertEqual(worker_post["code_update_strategy"], {"mode": "immediate"})
        self.assertNotIn("force", json.dumps(worker_post))
        image_post = next(body for method, path, body in posts if "rollouts" in path
                          and not path.endswith("deployments"))
        self.assertEqual(image_post["strategy"], "rolling")
        self.assertEqual(image_post["step_percentage"], 100)
        # Image only: the deploy changes nothing else, and a full re-post
        # could revert a rotated secret.
        self.assertEqual(image_post["target_configuration"], {"image": good_image()})
        actions = [action for action, _, _ in calls]
        self.assertEqual(actions, ["status", "fence", "status", "deployment-takeover"])
        # The takeover pins the restored version, never a stale answer.
        self.assertEqual(calls[-1][2], GOOD_VERSION)
        self.assertEqual(evidence["restored_version"], GOOD_VERSION)
        self.assertEqual(evidence["from_version"], BAD_VERSION)
        self.assertEqual(state["serving"], GOOD_VERSION)

    def test_worker_leg_skipped_when_already_restored(self):
        routes, _ = self.routes(serving=GOOD_VERSION)
        control, calls = control_stub()
        rb, client, _ = make_rollback(routes, control=control)
        rb.run(GOOD_VERSION, good_image())
        posts = [call for call in client.calls if call[0] == "POST"]
        self.assertFalse(any("deployments" in path for _, path, _ in posts))
        self.assertTrue(any("rollouts" in path for _, path, _ in posts))

    def test_image_restore_accepts_the_steady_state_lag_shape(self):
        # Under durable_object scheduling the completed rollout can read
        # active:1/healthy:0 while the exact build serves ready; staging
        # verify accepts that shape, so the restore must not time out on it.
        routes, _ = self.routes()
        lag = rollout_row("r-new", good_image(), created="2026-10-15T01:00:00Z",
                          status="completed", target_version=9)
        lag["health"]["instances"] = {"active": 1, "healthy": 0, "failed": 0,
                                      "starting": 0, "scheduling": 0}
        self.assertTrue(base.active_lag(lag, good_image(), 9))
        routes[(f"GET", f"/containers/applications/{APP_ID}/rollouts/r-new")] = \
            lambda: {"success": True, "result": lag}
        control, _ = control_stub()
        rb, client, _ = make_rollback(routes, control=control)
        evidence = rb.run(GOOD_VERSION, good_image())
        self.assertEqual(evidence["restored_rollout_id"], "r-new")
        posts = [call for call in client.calls if call[0] == "POST"]
        self.assertTrue(any("rollouts" in path for _, path, _ in posts))

    def test_production_variant_fences_then_takes_over_with_epoch(self):
        routes, _ = self.routes()
        control, calls = control_stub()
        rb, client, _ = make_rollback(routes, control=control, variant="production")
        rb.take_over(GOOD_VERSION)
        self.assertEqual([(action, epoch) for action, epoch, _ in calls],
                         [("status", None), ("fence", 9), ("status", None), ("takeover", 9)])
        self.assertEqual(calls[-1][2], GOOD_VERSION)

    def test_failed_restore_leaves_fixed_vocabulary_only(self):
        routes, _ = self.routes()
        # The replacement rollout is superseded mid-flight: fail closed.
        routes[(f"GET", f"/containers/applications/{APP_ID}/rollouts/r-new")] = \
            lambda: {"success": True, "result": dict(
                rollout_row("r-new", good_image(),
                            created="2026-10-15T01:00:00Z", target_version=9),
                status="replaced")}
        control, _ = control_stub()
        rb, _, log = make_rollback(routes, control=control)
        with self.assertRaises(base.GateError) as ctx:
            rb.run(GOOD_VERSION, good_image())
        self.assertEqual(str(ctx.exception), "rollout_replaced_or_reverted")
        self.assertNotIn("bearer", log.getvalue().lower())


class MainValidationTests(unittest.TestCase):
    def run_main(self, argv, environ):
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            code = rollback.main(argv, environ=environ)
        return code, stdout.getvalue()

    def base_env(self, **extra):
        env = {"CLOUDFLARE_ACCOUNT_ID": ACCOUNT, "CLOUDFLARE_API_TOKEN": "token",
               "OWNERSHIP_CONTROL_TOKEN": "t" * 32, "GITHUB_RUN_ID": "700"}
        env.update(extra)
        return env

    def test_needs_exactly_one_restore_source(self):
        url = "https://two-bot-next-staging.acct.workers.dev"
        cases = [
            ["rollback", "--variant", "staging", "--worker-url", url,
             "--actor", "a:b", "--evidence", "e.json"],
            ["rollback", "--variant", "staging", "--worker-url", url,
             "--snapshot", "s.json", "--target-version", GOOD_VERSION,
             "--actor", "a:b", "--evidence", "e.json"],
        ]
        for argv in cases:
            with self.subTest(argv=argv[-3:]):
                code, out = self.run_main(argv, self.base_env())
                self.assertEqual(code, 1)
                self.assertIn("either --snapshot or --target-version with --target-image",
                              out)
        code, out = self.run_main(
            ["rollback", "--variant", "staging", "--worker-url", url,
             "--target-version", GOOD_VERSION,
             "--actor", "a:b", "--evidence", "e.json"], self.base_env())
        self.assertEqual(code, 1)
        self.assertIn("--target-version needs --target-image", out)

    def test_rejects_bad_identity_before_any_call(self):
        url = "https://two-bot-next-staging.acct.workers.dev"
        code, out = self.run_main(
            ["rollback", "--variant", "staging", "--worker-url", url,
             "--snapshot", "s.json", "--expected-sha", "zzz",
             "--actor", "a:b", "--evidence", "e.json"], self.base_env())
        self.assertEqual(code, 1)
        self.assertIn("invalid_expected_sha", out)


class ProvenanceReceiptTests(unittest.TestCase):
    def receipt(self, **overrides):
        fields = {"worker": WORKER, "application": APPLICATION,
                  "version": GOOD_VERSION, "sha": SHA, "image": good_image()}
        fields.update(overrides)
        return rollback.ProvenanceReceipt(**fields)

    def test_complete_tuple_builds_and_validates(self):
        receipt = self.receipt()
        receipt.validate(version=GOOD_VERSION, sha=SHA, image=good_image())

    def test_missing_or_malformed_members_refuse_construction(self):
        cases = [
            ({"worker": ""}, "provenance_worker_invalid"),
            ({"worker": None}, "provenance_worker_invalid"),
            ({"application": ""}, "provenance_application_invalid"),
            ({"version": None}, "provenance_version_invalid"),
            ({"version": "not-a-uuid"}, "provenance_version_invalid"),
            ({"sha": None}, "provenance_sha_invalid"),
            ({"sha": ""}, "provenance_sha_invalid"),
            ({"sha": SHA[:39]}, "provenance_sha_invalid"),
            ({"sha": SHA.upper()}, "provenance_sha_invalid"),
            ({"image": None}, "provenance_image_invalid"),
            ({"image": good_image().replace(APPLICATION, "other-app")},
             "provenance_image_invalid"),
            ({"image": good_image().replace("@sha256:", ":latest@sha256:")},
             "provenance_image_invalid"),
        ]
        for overrides, code in cases:
            with self.subTest(overrides=str(overrides)[:60]):
                with self.assertRaises(base.GateError) as raised:
                    self.receipt(**overrides)
                self.assertEqual(str(raised.exception), code)

    def test_mismatched_inputs_refuse_and_name_the_member(self):
        receipt = self.receipt()
        cases = [
            ({"version": BAD_VERSION, "sha": SHA, "image": good_image()}, "member=version"),
            ({"version": GOOD_VERSION, "sha": "f" * 40, "image": good_image()}, "member=sha"),
            ({"version": GOOD_VERSION, "sha": SHA, "image": bad_image()}, "member=image"),
            ({"version": GOOD_VERSION, "sha": None, "image": good_image()}, "member=sha"),
        ]
        for inputs, member in cases:
            with self.subTest(member=member):
                with self.assertRaises(base.GateError) as raised:
                    receipt.validate(**inputs)
                self.assertEqual(str(raised.exception), "provenance_mismatch")
                self.assertEqual(raised.exception.detail, member)

    def test_receipt_is_immutable(self):
        receipt = self.receipt()
        for member in ("worker", "application", "version", "sha", "image"):
            with self.subTest(member=member):
                with self.assertRaises(dataclasses.FrozenInstanceError):
                    setattr(receipt, member, "tampered")
        self.assertEqual(receipt.sha, SHA)


class MainProvenanceTests(unittest.TestCase):
    """Manual and snapshot rollbacks refuse a bad receipt before any POST."""

    URL = "https://two-bot-next-staging.acct.workers.dev"

    def base_env(self):
        return {"CLOUDFLARE_ACCOUNT_ID": ACCOUNT, "CLOUDFLARE_API_TOKEN": "token",
                "OWNERSHIP_CONTROL_TOKEN": "t" * 32, "GITHUB_RUN_ID": "700"}

    def run_main(self, argv, client):
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            code = rollback.main(argv, environ=self.base_env(),
                                 client_factory=lambda _a, _t: client)
        return code, stdout.getvalue()

    def manual_argv(self, *extra):
        return ["rollback", "--variant", "staging", "--worker-url", self.URL,
                "--target-version", GOOD_VERSION, "--target-image", good_image(),
                "--actor", "a:b", "--evidence", "e.json", *extra]

    def live_routes(self, message=SHA, history=(BAD_VERSION, GOOD_VERSION),
                    served_image=None):
        served_image = good_image() if served_image is None else served_image
        deployments = [{"versions": [{"version_id": v, "percentage": 100}]}
                       for v in history]
        return {
            ("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"):
                version_route(message=message),
            ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
                "success": True, "result": {"deployments": deployments}},
            ("GET", "/containers/applications"): lambda: {
                "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                             "durable_objects": {"namespace_id": NAMESPACE}}]},
            ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"): lambda: {
                "success": True, "result": [rollout_row("r-prev", served_image)]},
        }

    def assert_refused_before_posts(self, argv, routes, code_text):
        client = FakeClient(routes)
        code, out = self.run_main(argv, client)
        self.assertEqual(code, 1)
        self.assertIn(code_text, out)
        self.assertFalse([call for call in client.calls if call[0] == "POST"],
                         client.calls)

    def test_manual_rollback_needs_the_receipt_sha(self):
        self.assert_refused_before_posts(self.manual_argv(), self.live_routes(),
                                         "manual_rollback_needs_expected_sha")

    def test_manual_rollback_refuses_a_sha_the_version_was_not_built_from(self):
        # The dispatch SHA is CI-green-shaped but is not this version's
        # commit: the version annotation refuses the pairing.
        argv = self.manual_argv("--expected-sha", "f" * 40)
        self.assert_refused_before_posts(argv, self.live_routes(), "provenance_mismatch")

    def test_manual_rollback_refuses_a_pre_stamp_version(self):
        argv = self.manual_argv("--expected-sha", SHA)
        self.assert_refused_before_posts(argv, self.live_routes(message=None),
                                         "provenance_mismatch")

    def test_manual_rollback_refuses_a_version_that_never_served(self):
        argv = self.manual_argv("--expected-sha", SHA)
        self.assert_refused_before_posts(
            argv, self.live_routes(history=(BAD_VERSION,)),
            "provenance_version_never_served")

    def test_manual_rollback_refuses_an_image_that_never_served(self):
        argv = self.manual_argv("--expected-sha", SHA)
        self.assert_refused_before_posts(
            argv, self.live_routes(served_image=bad_image()),
            "provenance_image_never_served")

    def test_manual_rollback_refuses_an_image_from_a_rollout_that_never_served(self):
        # The only rollout carrying the image never completed: without the
        # served-before filter it would pass as "served" and traffic would
        # move to an image from a failed, reverted or still-running rollout.
        for status in ("failed", "reverted", "pending", "progressing"):
            with self.subTest(status=status):
                served = rollout_row("r-prev", good_image(), status=status)
                routes = self.live_routes()
                routes[("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100")] = \
                    lambda served=served: {"success": True, "result": [served]}
                argv = self.manual_argv("--expected-sha", SHA)
                self.assert_refused_before_posts(argv, routes,
                                                 "provenance_image_never_served")

    def test_complete_receipt_validates_against_live_state(self):
        receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                             SHA, good_image())
        rollback.validate_receipt_against_live(FakeClient(self.live_routes()), receipt)

    def test_complete_receipt_validates_when_the_image_rollout_was_superseded(self):
        # Cloudflare flips a superseded rollout from completed to replaced,
        # so after any deploy the previous image lives only in a replaced
        # rollout. Served-before membership must accept it: the 10-20
        # production drill refused its snapshot image here and restored
        # nothing (provenance_image_never_served).
        receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                             SHA, good_image())
        routes = self.live_routes()
        routes[("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100")] = \
            lambda: {"success": True, "result": [
                rollout_row("r-newer", bad_image(),
                            created="2026-10-15T01:00:00Z", status="completed"),
                rollout_row("r-prev", good_image(), status="replaced")]}
        rollback.validate_receipt_against_live(FakeClient(routes), receipt)

    def test_receipt_refuses_an_image_from_a_replaced_rollout_still_in_flight(self):
        # A rollout can be replaced before it finishes (superseded in
        # flight), so a replaced row proves the image served only with
        # convergence evidence: every step completed, every instance
        # updated. The in-flight shape refuses like a never-served image.
        receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                             SHA, good_image())
        routes = self.live_routes()
        served = unconverged_row("r-prev", good_image())
        routes[("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100")] = \
            lambda served=served: {"success": True, "result": [served]}
        with self.assertRaises(rollback.GateError) as caught:
            rollback.validate_receipt_against_live(FakeClient(routes), receipt)
        self.assertEqual(str(caught.exception), "provenance_image_never_served")

    def test_receipt_refuses_a_replaced_rollout_without_convergence_evidence(self):
        # Any malformed convergence shape reads as unconverged: fail
        # closed instead of trusting status alone.
        shapes = {
            "no steps": {"steps": []},
            "step pending": {"steps": [{"status": "pending"}]},
            "step count mismatch": {
                "steps": [{"status": "completed"}],
                "progress": {"total_steps": 2, "current_step": 1,
                             "total_instances": 1, "updated_instances": 1}},
            "instances lagging": {
                "progress": {"total_steps": 1, "current_step": 1,
                             "total_instances": 2, "updated_instances": 1}},
            "no instances": {
                "progress": {"total_steps": 1, "current_step": 1,
                             "total_instances": 0, "updated_instances": 0}},
            "progress missing": {"progress": {}},
        }
        for name, shape in shapes.items():
            with self.subTest(name=name):
                receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                                     SHA, good_image())
                routes = self.live_routes()
                served = rollout_row("r-prev", good_image(), status="replaced",
                                     **shape)
                routes[("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100")] = \
                    lambda served=served: {"success": True, "result": [served]}
                with self.assertRaises(rollback.GateError) as caught:
                    rollback.validate_receipt_against_live(FakeClient(routes), receipt)
                self.assertEqual(str(caught.exception),
                                 "provenance_image_never_served")

    def snapshot_argv(self, path, *extra):
        return ["rollback", "--variant", "staging", "--worker-url", self.URL,
                "--snapshot", str(path), "--actor", "a:b", "--evidence", "e.json",
                *extra]

    def write_snapshot(self, tmp, **overrides):
        state = {"has_previous": True, "worker": WORKER, "application": APPLICATION,
                 "application_id": APP_ID, "namespace_id": NAMESPACE,
                 "serving_version": GOOD_VERSION, "serving_sha": SHA,
                 "provenance_complete": True, "serving_rollout_id": "r-prev",
                 "serving_image": good_image(), "recorded_at": 1.0}
        state.update(overrides)
        path = Path(tmp) / "snapshot.json"
        path.write_text(json.dumps(state))
        return path

    def snapshot_routes(self, image=None):
        image = good_image() if image is None else image
        return {("GET", f"/containers/applications/{APP_ID}/rollouts/r-prev"):
                lambda: {"success": True, "result": rollout_row("r-prev", image)}}

    def test_snapshot_rollback_refuses_image_drift_since_the_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write_snapshot(tmp)
            self.assert_refused_before_posts(self.snapshot_argv(path),
                                             self.snapshot_routes(bad_image()),
                                             "provenance_mismatch")

    def test_snapshot_rollback_refuses_a_conflicting_expected_sha(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write_snapshot(tmp)
            argv = self.snapshot_argv(path, "--expected-sha", "f" * 40)
            self.assert_refused_before_posts(argv, self.snapshot_routes(),
                                             "provenance_mismatch")

    def test_snapshot_rollback_refuses_a_malformed_recorded_sha(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write_snapshot(tmp, serving_sha="zzz")
            self.assert_refused_before_posts(self.snapshot_argv(path),
                                             self.snapshot_routes(),
                                             "invalid_snapshot_sha")

    def test_snapshot_rollback_revalidates_the_receipt_against_live_state(self):
        # A stale or hand-edited snapshot whose recorded SHA is not the
        # version's stamped deploy SHA refuses before any POST, exactly
        # like a manual rollback: never at the stable gate after restore.
        routes = self.snapshot_routes()
        routes[("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}")] = \
            version_route(message="f" * 40)
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write_snapshot(tmp)
            self.assert_refused_before_posts(self.snapshot_argv(path), routes,
                                             "provenance_mismatch")

    def snapshot_run_routes(self, revision):
        """Full main() snapshot-path routes: the recorded receipt
        revalidates, the worker leg is skipped (the snapshot names the
        serving version), the image leg posts, and the gate polls the
        restored build reporting `revision`."""
        handler, health = ready_probe(GOOD_VERSION, revision=revision)
        created = {"n": 0}

        def post_rollouts(body):
            created["n"] += 1
            return {"id": "r-new", "target_version": 9}

        def get_rollout():
            return {"success": True, "result": rollout_row(
                "r-new", good_image(), created="2026-10-15T01:00:00Z",
                status="completed", target_version=9)}

        deployments = [{"versions": [{"version_id": v, "percentage": 100}]}
                       for v in (GOOD_VERSION, BAD_VERSION)]
        return {
            ("GET", f"/containers/applications/{APP_ID}/rollouts/r-prev"):
                lambda: {"success": True, "result": rollout_row("r-prev", good_image())},
            ("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"):
                version_route(message=SHA),
            ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
                "success": True, "result": {"deployments": deployments}},
            ("GET", "/containers/applications"): lambda: {
                "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                             "durable_objects": {"namespace_id": NAMESPACE},
                                             "configuration": {"image": good_image()}}]},
            ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"): lambda: {
                "success": True, "result": [rollout_row("r-prev", good_image())]},
            ("POST", f"/containers/applications/{APP_ID}/rollouts"): post_rollouts,
            ("GET", f"/containers/applications/{APP_ID}/rollouts/r-new"): get_rollout,
            ("PROBE", "readyz"): handler,
            ("PROBE", "health"): health,
        }

    def run_snapshot_main(self, tmp, revision, route_overrides=None):
        """Run main() on a complete snapshot with a stubbed ownership
        client and a scripted clock; capture the gate's expected SHA."""
        routes = self.snapshot_run_routes(revision)
        routes.update(route_overrides or {})
        client = FakeClient(routes)
        control, _ = control_stub()
        clock = Clock()
        captured = {}
        real_cls = rollback.Rollback

        def spy_factory(*args, **kwargs):
            kwargs["now"] = clock.now
            kwargs["sleep"] = clock.sleep
            instance = real_cls(*args, **kwargs)
            captured["expected_sha"] = instance.expected_sha
            return instance

        path = self.write_snapshot(tmp)
        evidence = str(Path(tmp) / "evidence.json")
        argv = ["rollback", "--variant", "staging", "--worker-url", self.URL,
                "--snapshot", str(path), "--actor", "a:b",
                "--evidence", evidence]
        stdout = io.StringIO()
        with (mock.patch.object(rollback, "Rollback", spy_factory),
              mock.patch.object(rollback, "control_client",
                                lambda *args, **kwargs: control),
              redirect_stdout(stdout)):
            code = rollback.main(argv, environ=self.base_env(),
                                 client_factory=lambda _a, _t: client)
        return code, stdout.getvalue(), captured, client, evidence

    def test_snapshot_rollback_pins_the_gate_to_the_recorded_sha(self):
        # The receipt SHA becomes the stable gate's expected revision, so
        # the restored build must report it. Deleting the pin leaves the
        # gate unpinned (None) and this assertion fails.
        with tempfile.TemporaryDirectory() as tmp:
            code, out, captured, client, evidence = self.run_snapshot_main(tmp, SHA)
            self.assertEqual(code, 0, out)
            self.assertEqual(captured.get("expected_sha"), SHA)
            record = json.loads(Path(evidence).read_text())
        self.assertEqual(record["provenance"], "validated")
        self.assertEqual(record["provenance_sha"], SHA)
        self.assertTrue([call for call in client.calls if call[0] == "POST"],
                        client.calls)

    def test_snapshot_rollback_restores_when_the_snapshot_rollout_was_superseded(self):
        # Drill shape: the deploy completed a newer rollout, so the
        # snapshot's rollout now reads replaced (same image) and the list
        # carries the newer completed rollout beside it. The rollback must
        # still validate the receipt and restore the snapshot image.
        overrides = {
            ("GET", f"/containers/applications/{APP_ID}/rollouts/r-prev"):
                lambda: {"success": True, "result": rollout_row(
                    "r-prev", good_image(), status="replaced")},
            ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"):
                lambda: {"success": True, "result": [
                    rollout_row("r-newer", bad_image(),
                                created="2026-10-15T01:00:00Z",
                                status="completed", target_version=8),
                    rollout_row("r-prev", good_image(), status="replaced")]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            code, out, _, client, evidence = self.run_snapshot_main(
                tmp, SHA, overrides)
            self.assertEqual(code, 0, out)
            record = json.loads(Path(evidence).read_text())
        self.assertEqual(record["provenance"], "validated")
        self.assertEqual(record["restored_image_digest"], "sha256:" + "a" * 64)
        self.assertTrue([call for call in client.calls if call[0] == "POST"],
                        client.calls)

    def test_snapshot_rollback_refuses_when_the_snapshot_rollout_never_converged(self):
        # Snapshot path of the in-flight replaced shape: the recorded
        # image matches the live re-read (no drift), but the row never
        # converged, so served-before membership still refuses before
        # any POST.
        row = unconverged_row("r-prev", good_image())
        overrides = {
            ("GET", f"/containers/applications/{APP_ID}/rollouts/r-prev"):
                lambda row=row: {"success": True, "result": row},
            ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"):
                lambda row=row: {"success": True, "result": [row]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            code, out, _, client, _ = self.run_snapshot_main(tmp, SHA, overrides)
            self.assertEqual(code, 1, out)
            self.assertIn("provenance_image_never_served", out)
        self.assertFalse([call for call in client.calls if call[0] == "POST"],
                         client.calls)

    def test_snapshot_rollback_gate_refuses_a_revision_other_than_the_receipt_sha(self):
        # Without the pin this answer passes on the version header alone;
        # with it the gate polls to its deadline and fails closed.
        with tempfile.TemporaryDirectory() as tmp:
            code, out, captured, _, _ = self.run_snapshot_main(tmp, "f" * 40)
        self.assertEqual(code, 1)
        self.assertEqual(captured.get("expected_sha"), SHA)
        self.assertIn("gate_timeout", out)


class PostStatusTests(unittest.TestCase):
    """A created rollout (201) is success; the 10-16 staging auto-rollback
    posted its rollout, read 201 and failed the run as api_http_failure."""

    class Response:
        def __init__(self, status, body):
            self.status, self.body = status, body

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self, limit):
            return self.body[:limit]

    def client(self, status):
        body = json.dumps({"success": True, "result": {"id": "r-1"}}).encode()
        client = rollback.RollbackClient(ACCOUNT, "token")
        client.opener = type("Opener", (), {"open": lambda _self, request, timeout: self.Response(status, body)})()
        return client

    def test_created_201_is_success(self):
        self.assertEqual(self.client(201).post("/x", {}), {"id": "r-1"})

    def test_ok_200_is_success(self):
        self.assertEqual(self.client(200).post("/x", {}), {"id": "r-1"})

    def test_other_2xx_fails_closed(self):
        with self.assertRaises(base.GateError) as raised:
            self.client(202).post("/x", {})
        self.assertEqual(str(raised.exception), "api_http_failure")


class ControlFenceHintTests(unittest.TestCase):
    FENCED = ("Singleton is intentionally fenced or uninitialized; "
              "explicit staging release required\n")

    def refusal(self, stderr, variant="staging", action="takeover", epoch=None):
        failed = rollback.subprocess.CompletedProcess(args=[], returncode=1, stdout="", stderr=stderr)
        control = rollback.control_client(variant, "secret-token", "https://w.example",
                                          "https://s.example", "actor")
        with mock.patch.object(rollback.subprocess, "run", return_value=failed):
            with self.assertRaises(base.GateError) as raised:
                control(action, epoch)
        return raised.exception

    def test_fenced_refusal_carries_no_release_hint(self):
        # The client must stay hint-free: its takeover calls run with
        # OWNERSHIP_RELEASE_FENCE=true (pinned below), so the fenced refusal
        # is unreachable through them, and production throws the same prefix
        # with a different release route. The scoped fence guidance lives in
        # the deploy-staging takeover step instead.
        error = self.refusal(self.FENCED)
        self.assertEqual(str(error), "ownership_control_failed")
        # Only the allowlisted prefix reaches the detail, with no hint.
        self.assertEqual(error.detail, "Singleton is intentionally fenced or uninitialized")
        self.assertNotIn("secret-token", error.detail)

    def test_production_fenced_refusal_names_no_staging_route(self):
        error = self.refusal(self.FENCED, variant="production", action="fence", epoch=9)
        self.assertEqual(str(error), "ownership_control_failed")
        self.assertNotIn("deploy-staging", error.detail)
        self.assertNotIn("release_fence", error.detail)

    def test_other_refusals_carry_no_release_hint(self):
        error = self.refusal("Ownership transition not confirmed\n")
        self.assertEqual(error.detail, "Ownership transition not confirmed")

    def test_transient_control_lines_reach_the_detail(self):
        # The production client's classified request failures are fixed
        # vocabulary with integers only, so the rollback log may carry the
        # cause instead of dropping it: attempts/elapsed stay out.
        cases = [
            ("Ownership control request timeout attempts=3 elapsed=45s; stop, do not change credentials\n",
             "Ownership control request timeout"),
            ("Ownership control request failed attempts=1 elapsed=2s; stop, do not change credentials\n",
             "Ownership control request failed"),
            ("Ownership control invalid response attempts=2 elapsed=12s; stop, do not change credentials\n",
             "Ownership control invalid response"),
        ]
        for stderr, prefix in cases:
            with self.subTest(prefix=prefix):
                error = self.refusal(stderr, variant="production", action="status")
                self.assertEqual(str(error), "ownership_control_failed")
                self.assertEqual(error.detail, prefix)
                self.assertNotIn("secret-token", error.detail)

    def test_fence_env_contract_that_makes_the_hint_dead(self):
        # Pins the env contract behind the hint removal: takeover-style
        # actions release the fence, fence/status do not. A fenced refusal
        # can therefore never answer this client's takeover calls.
        seen = {}

        def fake_run(command, **kwargs):
            seen[command[2]] = kwargs["env"]["OWNERSHIP_RELEASE_FENCE"]
            return rollback.subprocess.CompletedProcess(args=command, returncode=0,
                                                       stdout='{"ok": true}', stderr="")

        control = rollback.control_client("staging", "t" * 32, "https://w.example",
                                          "https://s.example", "actor")
        with mock.patch.object(rollback.subprocess, "run", side_effect=fake_run):
            control("deployment-takeover")
            control("takeover", 9)
            control("fence", 9)
            control("status")
        self.assertEqual(seen, {"deployment-takeover": "true", "takeover": "true",
                                "fence": "false", "status": "false"})


class ControlDeepStdoutTests(unittest.TestCase):
    # Depth-10000 nested objects exhaust the decoder recursion limit (the
    # probe suite's nested_object_body precedent). The hooked status parse
    # must map that to the fixed malformed-stdout refusal, not a traceback.
    DEEP_STATUS = '{"a":' * 10000 + "1" + "}" * 10000

    def control(self, stdout):
        control = rollback.control_client(
            "staging", "t" * 32, "https://w.example", "https://s.example", "actor")
        done = rollback.subprocess.CompletedProcess(
            args=[], returncode=0, stdout=stdout, stderr="")
        patcher = mock.patch.object(rollback.subprocess, "run", return_value=done)
        patcher.start()
        self.addCleanup(patcher.stop)
        return control

    def test_deeply_nested_status_refuses_as_invalid_response(self):
        with self.assertRaises(base.GateError) as caught:
            self.control(self.DEEP_STATUS)("status")
        self.assertEqual(str(caught.exception), "ownership_control_invalid_response")
        self.assertIsNone(caught.exception.detail)

    def test_deeply_nested_status_refuses_before_any_restore_post(self):
        # BAD serves and GOOD is the restore target, so a parseable status
        # would fence first and then POST; the deep stdout refuses instead.
        routes = {("GET", f"/workers/scripts/{WORKER}/deployments"):
                  lambda: {"success": True, "result": {"deployments": [
                      {"versions": [{"version_id": BAD_VERSION,
                                     "percentage": 100}]}]}}}
        rb, client, _ = make_rollback(routes, control=self.control(self.DEEP_STATUS))
        with self.assertRaises(base.GateError) as caught:
            rb.run(GOOD_VERSION, good_image())
        self.assertEqual(str(caught.exception), "ownership_control_invalid_response")
        self.assertEqual([call for call in client.calls if call[0] == "POST"], [])


if __name__ == "__main__":
    unittest.main()

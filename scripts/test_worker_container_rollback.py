"""Offline pins for scripts/worker_container_rollback.py.

No Cloudflare or ownership credentials: the control plane is faked in
memory, time is scripted, and the ownership client is a recording stub.
Nothing here dispatches a workflow or moves traffic.
"""

import io
import json
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path

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
        }
        state = rollback.snapshot_state(FakeClient(routes), WORKER, APPLICATION)
        self.assertEqual(state, {"has_previous": True, "worker": WORKER,
                                 "application": APPLICATION, "application_id": APP_ID,
                                 "namespace_id": NAMESPACE, "serving_version": GOOD_VERSION,
                                 "serving_rollout_id": "r-prev", "serving_image": good_image(),
                                 "recorded_at": state["recorded_at"]})
        # The previous target configuration is re-read at rollback time and
        # held in memory only: it must never reach the snapshot artifact.
        self.assertNotIn("target_configuration", json.dumps(state))

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


class RollbackPostTests(unittest.TestCase):
    def test_post_accepts_http_201_created_envelope(self):
        # Cloudflare creates a rollout with HTTP 201; the client must
        # return the result envelope instead of failing api_http_failure.
        payload = {"success": True, "result": {"id": "r-new"}}

        class FakeResponse:
            status = 201

            def read(self, _limit):
                return json.dumps(payload).encode()

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class FakeOpener:
            def open(self, _request, timeout=None):
                return FakeResponse()

        client = rollback.RollbackClient(ACCOUNT, "token")
        client.opener = FakeOpener()
        self.assertEqual(
            client.post(f"/containers/applications/{APP_ID}/rollouts", {"image": good_image()}),
            {"id": "r-new"})


class RollbackRunTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()

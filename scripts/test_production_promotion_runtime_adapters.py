"""Runtime-adapter acceptance for immutable production promotion.

Covers two seams of scripts/production_deploy.py without touching the
implementation:

* ``serving`` — GET-only control-plane proof that the production-local
  promoted image has converged under the exact current production Worker
  version and target application version, with the Worker re-read after
  image convergence. The proof binds the original admitted receipt and an
  independent canonical production deployment event. Identity-only preflight
  must unblock cold starts; either completed singleton counter-lag shape needs
  two consecutive exact live /health + /readyz confirmations. A predeploy
  snapshot authorizes polling only its exact old rollout/image, never a newer
  foreign rollout. The legacy no-URL strict-convergence path stays covered.
* ``readyz`` with ``--artifact`` — the verdict must come from the actual
  ``/health`` AND ``/readyz`` response headers (exact
  ``x-two-worker-version``), while the build stamps compared are the
  admitted ORIGINAL staging stamps, not this production event's IDs.
  Legacy no-artifact verdicts and the body cap are preserved.

Boundaries substituted (strict doubles, never the methods under test):
only the existing rollback/base client external I/O boundary
(``staging_rollout.Client``, reached as
``worker_container_rollback.base.Client``), the clock
(``time.monotonic``/``time.sleep``), the process environment (Cloudflare
credential names), and local fixture files. ``load_admitted``,
``serving_version``, ``application_by_name``, ``app_rollouts``,
``rollout_image``, ``converged``, ``response_version``, ``serving`` and
``readyz`` themselves are never patched or reimplemented.

Offline: any subprocess or real network access fails the test outright.
"""

import contextlib
import copy
import io
import json
import os
import sys
import tempfile
import unittest
from email.message import Message
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

import production_deploy
import staging_rollout as base
import test_immutable_bot_promotion as promo
import worker_container_rollback as rollback

ACCOUNT = "a" * 32
TOKEN = "token-prod"
APP_ID = "app-prod-1"
NAMESPACE = "ns-prod-1"
ROLL_ID = "roll-prod-1"
TARGET_VERSION = 7
PROD_APP = "two-bot-next-twobotcontainer-production"
PROD_WORKER = "two-bot-next-production"
CREATED = "2026-10-15T00:00:00Z"
NEWER_CREATED = "2026-10-16T00:00:00Z"
OTHER_VERSION = "33333333-3333-4333-8333-333333333333"
OTHER_DIGEST = "sha256:" + "d" * 64

SHA = promo.SHA
BUILD = promo.BUILD
STAMP = promo.STAMP
EVENT = promo.EVENT
STAGE_VERSION = promo.STAGE_VERSION
PROD_VERSION = promo.PROD_VERSION

# The approved account is ambient (CLOUDFLARE_ACCOUNT_ID) and namespaced:
# the admitted staging/production images must live in that same account,
# so the normal fixture derives both refs from ACCOUNT instead of reusing
# the sibling suite's "acct" registry namespace. The sibling suite's
# refs double as the foreign account below.
DIGEST_C = "sha256:" + "c" * 64
PROD_REPO = f"registry.cloudflare.com/{ACCOUNT}/two-bot-next-twobotcontainer-production"
PROD_IMAGE = PROD_REPO + "@" + DIGEST_C
STAGE_IMAGE = (f"registry.cloudflare.com/{ACCOUNT}/"
               "two-bot-next-twobotcontainer-staging@" + DIGEST_C)
FOREIGN_PROD_IMAGE = promo.PROD_IMAGE
FOREIGN_STAGE_IMAGE = promo.STAGE_IMAGE


def admitted_receipt():
    """Sibling receipt shape with images in the ambient approved account."""
    record = copy.deepcopy(promo.receipt())
    for holder in (record["container"], record["staging"]):
        holder["image"] = holder["image"].replace("/acct/", f"/{ACCOUNT}/")
    return record


def converged_row(rollout_id, image, created=CREATED, target_version=TARGET_VERSION):
    return {"id": rollout_id, "created_at": created, "status": "completed",
            "strategy": "rolling", "kind": "full_auto",
            "target_version": target_version,
            "current_version": target_version - 1,
            "target_configuration": {"image": image, "instance_type": "lite"},
            "current_configuration": {"image": image},
            "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                     "starting": 0, "scheduling": 0}},
            "progress": {"total_steps": 1, "current_step": 1,
                         "total_instances": 1, "updated_instances": 1},
            "steps": [{"status": "completed"}]}


def progressing_row(rollout_id, image, created=CREATED):
    row = converged_row(rollout_id, image, created)
    row["status"] = "progressing"
    row["progress"] = {"total_steps": 1, "current_step": 1,
                       "total_instances": 1, "updated_instances": 0}
    row["steps"] = [{"status": "in_progress"}]
    return row


class FakeControlPlane:
    """GET-only Cloudflare stand-in on the existing client boundary.

    ``post`` is absent by design: any POST attempt raises, proving the
    serving proof path is read-only.
    """

    def __init__(self, routes, calls):
        self.routes = routes
        self.calls = calls

    def api(self, path):
        self.calls.append(path)
        try:
            handler = self.routes[path]
        except KeyError:
            raise AssertionError("unexpected control-plane read: " + path) from None
        return handler()

    def post(self, *args, **kwargs):
        raise AssertionError("serving must not POST")


class FakeClock:
    def __init__(self):
        self.t = 1000.0
        self.sleeps = []

    def now(self):
        return self.t

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.t += seconds


def deployments_answer(version):
    return {"deployments": [{"versions": [{"version_id": version,
                                           "percentage": 100}]}]}


def app_answer(image=PROD_IMAGE, version=TARGET_VERSION):
    return [{"id": APP_ID, "name": PROD_APP,
             "durable_objects": {"namespace_id": NAMESPACE},
             "configuration": {"image": image}, "version": version}]


class ServingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.artifact = self.root / "admitted.json"
        self.artifact.write_text(json.dumps(
            {"schema_version": 1, "receipt": admitted_receipt(),
             "production_image": PROD_IMAGE}))
        self.out = self.root / "serving-proof.json"
        for target in ("subprocess.run", "subprocess.Popen",
                       "socket.create_connection", "urllib.request.urlopen"):
            guard = mock.patch(target, side_effect=AssertionError(
                "unexpected subprocess/network: " + target))
            guard.start()
            self.addCleanup(guard.stop)

    def run_serving(self, routes, deploy_id=EVENT,
                    expected_version=PROD_VERSION, clock=None):
        clock = clock or FakeClock()
        calls = []
        plane = FakeControlPlane(routes, calls)
        output = io.StringIO()
        with (mock.patch.dict(os.environ, {"CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
                                            "CLOUDFLARE_API_TOKEN": TOKEN}),
              mock.patch.object(rollback.base, "Client",
                                side_effect=lambda account, token, deadline=None: (
                                    self.assertEqual(account, ACCOUNT),
                                    self.assertEqual(token, TOKEN),
                                    plane)[-1]),
              mock.patch("time.monotonic", clock.now),
              mock.patch("time.sleep", clock.sleep),
              contextlib.redirect_stdout(output),
              contextlib.redirect_stderr(output)):
            try:
                code = production_deploy.main([
                    "serving", "--sha", SHA, "--deploy-id", deploy_id,
                    "--artifact", str(self.artifact),
                    "--expected-worker-version", expected_version,
                    "--out", str(self.out)])
            except SystemExit as exc:
                code = exc.code
        proof = json.loads(self.out.read_text()) if self.out.exists() else None
        return code, output.getvalue(), proof, calls, clock

    def live_routes(self, row, app_image=PROD_IMAGE, app_version=TARGET_VERSION,
                    versions=None):
        reads = {"deployments": 0}

        def deployments():
            reads["deployments"] += 1
            version = versions[reads["deployments"] - 1] if versions \
                else PROD_VERSION
            return deployments_answer(version)

        return {
            f"/workers/scripts/{PROD_WORKER}/deployments": deployments,
            "/containers/applications": lambda: app_answer(app_image,
                                                           app_version),
            f"/containers/applications/{APP_ID}/rollouts?limit=100":
                lambda: [dict(row, id=row["id"])] if isinstance(row, dict)
                else [dict(candidate) for candidate in row],
            f"/containers/applications/{APP_ID}/rollouts/{row['id'] if isinstance(row, dict) else row[0]['id']}":
                lambda: dict(row) if isinstance(row, dict) else dict(row[0]),
        }

    def test_converged_image_under_exact_worker_and_app_version_proves(self):
        routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE))
        code, message, proof, calls, _ = self.run_serving(routes)
        self.assertEqual(code, 0, message)
        self.assertIn("promoted production Worker and container digest verified",
                      message)
        self.assertIsNotNone(proof)
        self.assertEqual(proof["worker_version"], PROD_VERSION)
        self.assertEqual(proof["production_image"], PROD_IMAGE)
        # The proof binds this production event, independent of the staging
        # stamps it was admitted from.
        self.assertEqual(proof["production_deploy_id"], EVENT)
        self.assertNotEqual(EVENT, STAMP)
        self.assertEqual(proof["rollout_id"], ROLL_ID)
        self.assertEqual(proof["target_version"], TARGET_VERSION)
        self.assertEqual(proof["receipt"], admitted_receipt())
        # Same approved account throughout: the admitted images live in
        # the ambient API account, not a foreign registry namespace.
        self.assertIn("/" + ACCOUNT + "/", proof["production_image"])
        self.assertIn("/" + ACCOUNT + "/", proof["receipt"]["container"]["image"])
        # The Worker is read before AND after image convergence: no
        # secret-only intervening publish can authorize stale bytes.
        deployment_reads = [
            path for path in calls
            if path == f"/workers/scripts/{PROD_WORKER}/deployments"]
        self.assertEqual(len(deployment_reads), 2)
        self.assertTrue(calls)
        self.assertTrue(all(path.startswith("/containers/applications")
                            or path.startswith("/workers/scripts")
                            for path in calls))

    def test_wrong_or_new_worker_version_refuses_without_proof(self):
        for versions in ([OTHER_VERSION], [PROD_VERSION, OTHER_VERSION]):
            with self.subTest(versions=versions):
                routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE),
                                          versions=versions)
                code, message, proof, _, _ = self.run_serving(routes)
                self.assertNotEqual(code, 0, message)
                self.assertIn("refused", message)
                self.assertIsNone(proof)

    def test_staging_worker_version_is_never_production_proof(self):
        routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE),
                                  versions=[STAGE_VERSION])
        code, message, proof, _, _ = self.run_serving(routes)
        self.assertNotEqual(code, 0, message)
        self.assertIn("refused", message)
        self.assertIsNone(proof)

    def test_foreign_or_different_image_refuses_without_proof(self):
        for image in (STAGE_IMAGE, PROD_REPO + "@" + OTHER_DIGEST):
            with self.subTest(image=image):
                routes = self.live_routes(converged_row(ROLL_ID, image))
                code, message, proof, _, _ = self.run_serving(routes)
                self.assertNotEqual(code, 0, message)
                self.assertIn("refused", message)
                self.assertIsNone(proof)

    def test_missing_rollout_refuses_without_proof(self):
        routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE))
        routes[f"/containers/applications/{APP_ID}/rollouts?limit=100"] = \
            lambda: []
        code, message, proof, _, _ = self.run_serving(routes)
        self.assertNotEqual(code, 0, message)
        self.assertIn("refused", message)
        self.assertIsNone(proof)

    def assert_refused_without_proof(self, code, message, proof):
        self.assertNotEqual(code, 0, message)
        self.assertTrue(message.startswith("serving: promoted production "),
                        message)
        self.assertIsNone(proof)

    def test_nonconverged_rollout_never_proves(self):
        clock = FakeClock()

        def jump(seconds):
            clock.sleeps.append(seconds)
            clock.t += 1000.0

        clock.sleep = jump
        routes = self.live_routes(progressing_row(ROLL_ID, PROD_IMAGE))
        code, message, proof, _, _ = self.run_serving(routes, clock=clock)
        self.assert_refused_without_proof(code, message, proof)

    def test_latest_rollout_decides_not_an_older_converged_one(self):
        old = converged_row("roll-old", PROD_IMAGE, created=CREATED)
        new = progressing_row("roll-new", PROD_IMAGE, created=NEWER_CREATED)
        routes = {
            f"/workers/scripts/{PROD_WORKER}/deployments":
                lambda: deployments_answer(PROD_VERSION),
            "/containers/applications": lambda: app_answer(),
            f"/containers/applications/{APP_ID}/rollouts?limit=100":
                lambda: [dict(old), dict(new)],
            f"/containers/applications/{APP_ID}/rollouts/roll-new":
                lambda: dict(new),
        }
        code, message, proof, _, _ = self.run_serving(routes)
        self.assert_refused_without_proof(code, message, proof)

    def test_wrong_application_version_refuses_without_proof(self):
        routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE),
                                  app_version=TARGET_VERSION + 1)
        code, message, proof, _, _ = self.run_serving(routes)
        self.assert_refused_without_proof(code, message, proof)

    def test_foreign_account_admitted_image_never_proves(self):
        # Admission already binds the CLI account at admit time; this tests
        # serving's own same-account proof contract, not a normal-workflow
        # exploit claim. An admitted image from another registry namespace
        # must never prove, even when that foreign world is fully converged
        # under the expected Worker. Detection may validly happen after
        # (read-only) API reads, so only nonzero exit and no successful
        # proof are required; the GET-only boundary (FakeControlPlane.post
        # raises) holds regardless.
        self.artifact.write_text(json.dumps(
            {"schema_version": 1, "receipt": promo.receipt(),
             "production_image": FOREIGN_PROD_IMAGE}))
        routes = self.live_routes(
            converged_row(ROLL_ID, FOREIGN_PROD_IMAGE),
            app_image=FOREIGN_PROD_IMAGE)
        code, message, proof, _, _ = self.run_serving(routes)
        self.assert_refused_without_proof(code, message, proof)

    def test_wait_for_convergence_then_proves_with_final_worker_read(self):
        # A rollout that is still progressing on the first read and
        # converged on the next must prove: the gate waits (sleeps),
        # then binds the proof with a fresh Worker read after convergence.
        reads = {"detail": 0}

        def detail():
            reads["detail"] += 1
            if reads["detail"] == 1:
                return progressing_row(ROLL_ID, PROD_IMAGE)
            return converged_row(ROLL_ID, PROD_IMAGE)

        routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE))
        routes[f"/containers/applications/{APP_ID}/rollouts/{ROLL_ID}"] = detail
        clock = FakeClock()
        code, message, proof, calls, _ = self.run_serving(routes, clock=clock)
        self.assertEqual(code, 0, message)
        self.assertIsNotNone(proof)
        self.assertEqual(proof["worker_version"], PROD_VERSION)
        self.assertEqual(proof["production_image"], PROD_IMAGE)
        self.assertEqual(proof["production_deploy_id"], EVENT)
        self.assertEqual(proof["rollout_id"], ROLL_ID)
        self.assertEqual(proof["target_version"], TARGET_VERSION)
        self.assertEqual(proof["receipt"], admitted_receipt())
        self.assertTrue(clock.sleeps, "the gate must wait for convergence")
        deployment_reads = [
            path for path in calls
            if path == f"/workers/scripts/{PROD_WORKER}/deployments"]
        self.assertEqual(len(deployment_reads), 3,
                         "Worker read per poll plus the post-convergence re-read")

    def test_auth_error_and_timeout_refuse_without_proof(self):
        for failure in ("api_http_failure", "api_transport_failure"):
            with self.subTest(failure=failure):
                def fail():
                    raise base.GateError(failure)

                routes = {f"/workers/scripts/{PROD_WORKER}/deployments": fail,
                          "/containers/applications": fail,
                          f"/containers/applications/{APP_ID}/rollouts?limit=100":
                              fail}
                code, message, proof, _, _ = self.run_serving(routes)
                self.assertNotEqual(code, 0, message)
                self.assertIn("refused", message)
                self.assertIsNone(proof)
        with self.subTest(failure="transport"):
            def unreachable():
                raise OSError("connection reset")

            routes = {f"/workers/scripts/{PROD_WORKER}/deployments":
                      unreachable}
            code, message, proof, _, _ = self.run_serving(routes)
            self.assertNotEqual(code, 0, message)
            self.assertIn("refused", message)
            self.assertIsNone(proof)

    def test_noncanonical_or_foreign_production_event_refuses(self):
        routes = self.live_routes(converged_row(ROLL_ID, PROD_IMAGE))
        for deploy_id in ("2026.10.22.3+7654321", "not-a-deploy-id"):
            with self.subTest(deploy_id=deploy_id):
                code, message, proof, _, _ = self.run_serving(
                    routes, deploy_id=deploy_id)
                self.assert_refused_without_proof(code, message, proof)


RUNTIME_URL = "https://production.example.invalid"
OLD_IMAGE = PROD_REPO + "@" + OTHER_DIGEST


def runtime_answer(*, status=200, version=PROD_VERSION, report=None,
                   duplicate=False, dictionary=False):
    headers = {"x-two-worker-version": version} if dictionary else Message()
    if not dictionary:
        if version is not None:
            headers.add_header("x-two-worker-version", version)
        if duplicate:
            headers.add_header("x-two-worker-version", version)
    body = {"build_revision": SHA, "build_id": BUILD, "deploy_id": STAMP,
            "components": [["gateway", "ready"]]}
    if report:
        body.update(report)
    return status, headers, json.dumps(body).encode()


class RuntimeControlPlane(FakeControlPlane):
    """Only external GETs are replaced; serving/readyz/shape checks stay real."""

    def __init__(self, routes, calls, samples):
        super().__init__(routes, calls)
        self.samples = samples
        self.runtime_calls = []
        self.sample_index = 0
        self.seen = set()

    def request(self, url, authenticated=False):
        if authenticated or url not in (RUNTIME_URL + "/health", RUNTIME_URL + "/readyz"):
            raise AssertionError("unexpected runtime request: " + url)
        endpoint = url.rsplit("/", 1)[-1]
        if endpoint in self.seen or len(self.seen) == 2:
            self.sample_index += 1
            self.seen.clear()
        self.seen.add(endpoint)
        self.runtime_calls.append((self.sample_index, endpoint))
        self.calls.append(url)
        sample = self.samples[min(self.sample_index, len(self.samples) - 1)]
        response = sample.get(endpoint, runtime_answer())
        if isinstance(response, Exception):
            raise response
        return response


class ColdAndLagServingTests(unittest.TestCase):
    # Reuse fixture creation and external network guards, not any verdict.
    setUp = ServingTests.setUp
    live_routes = ServingTests.live_routes

    def attempt(self, row=None, *, preflight=False, url=RUNTIME_URL,
                samples=None, routes=None, snapshot=None, cli=False):
        self.out.unlink(missing_ok=True)
        routes = routes or self.live_routes(row or converged_row(ROLL_ID, PROD_IMAGE))
        clock = FakeClock()
        calls = []
        plane = RuntimeControlPlane(routes, calls, samples or [{}])
        args = SimpleNamespace(sha=SHA, deploy_id=EVENT,
                               artifact=str(self.artifact),
                               expected_worker_version=PROD_VERSION,
                               out=str(self.out), preflight=preflight,
                               url=url, snapshot=None)
        if snapshot is not None:
            path = self.root / "predeploy-snapshot.json"
            path.write_text(json.dumps(snapshot))
            args.snapshot = str(path)
        output = io.StringIO()
        with (mock.patch.dict(os.environ, {"CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
                                           "CLOUDFLARE_API_TOKEN": TOKEN}),
              mock.patch.object(rollback.base, "Client", return_value=plane),
              mock.patch("time.monotonic", clock.now),
              mock.patch("time.sleep", clock.sleep),
              contextlib.redirect_stdout(output), contextlib.redirect_stderr(output)):
            if cli:
                argv = ["serving", "--sha", SHA, "--deploy-id", EVENT,
                        "--artifact", str(self.artifact),
                        "--expected-worker-version", PROD_VERSION, "--out", str(self.out)]
                if preflight:
                    argv.append("--preflight")
                if url:
                    argv += ["--url", url]
                if args.snapshot:
                    argv += ["--snapshot", args.snapshot]
                try:
                    code = production_deploy.main(argv)
                except SystemExit as exc:
                    code = exc.code
            else:
                # Direct API proves behavioral RED even before CLI flags exist.
                code = production_deploy.serving(args)
        proof = json.loads(self.out.read_text()) if self.out.exists() else None
        return code, output.getvalue(), proof, plane, clock

    def lag_row(self, active=0, healthy=1):
        row = converged_row(ROLL_ID, PROD_IMAGE)
        row["health"]["instances"].update(active=active, healthy=healthy)
        return row

    def assert_refused(self, result):
        code, message, proof, _, _ = result
        self.assertEqual(code, 1, message)
        self.assertTrue(message.startswith("serving: promoted production "), message)
        self.assertNotIn("unrecognized arguments", message)
        self.assertIsNone(proof)

    def test_preflight_accepts_owned_cold_identity_without_start_or_runtime_probe(self):
        cold = self.lag_row(active=0, healthy=0)
        code, message, proof, plane, _ = self.attempt(cold, preflight=True, url=None)
        self.assertEqual(code, 0, message)
        self.assertEqual(proof["phase"], "identity")
        self.assertEqual(proof["production_image"], PROD_IMAGE)
        self.assertEqual(proof["worker_version"], PROD_VERSION)
        self.assertEqual(proof["receipt"], admitted_receipt())
        self.assertEqual(plane.runtime_calls, [], "identity must not start or probe a cold container")

    def test_preflight_cli_publishes_the_identity_handoff(self):
        code, message, proof, plane, _ = self.attempt(
            self.lag_row(active=0, healthy=0), preflight=True, url=None, cli=True)
        self.assertEqual(code, 0, message)
        self.assertEqual(proof["phase"], "identity")
        self.assertEqual(plane.runtime_calls, [])

    def test_preflight_still_refuses_wrong_worker_rollout_and_application(self):
        cases = [self.live_routes(converged_row(ROLL_ID, OLD_IMAGE)),
                 self.live_routes(converged_row(ROLL_ID, PROD_IMAGE), versions=[OTHER_VERSION]),
                 self.live_routes(converged_row(ROLL_ID, PROD_IMAGE), app_image=OLD_IMAGE),
                 self.live_routes(converged_row(ROLL_ID, PROD_IMAGE), app_version=TARGET_VERSION + 1)]
        for routes in cases:
            with self.subTest(routes=list(routes)):
                result = self.attempt(routes=routes, preflight=True, url=None)
                self.assert_refused(result)
                self.assertEqual(result[3].runtime_calls, [])

    def test_both_lag_shapes_need_two_exact_live_health_and_readyz_confirmations(self):
        for active, healthy in ((0, 1), (1, 0)):
            for dictionary in (False, True):
                with self.subTest(active=active, healthy=healthy, dictionary=dictionary):
                    sample = {endpoint: runtime_answer(dictionary=dictionary)
                              for endpoint in ("health", "readyz")}
                    code, message, proof, plane, clock = self.attempt(
                        self.lag_row(active, healthy), samples=[sample])
                    self.assertEqual(code, 0, message)
                    self.assertEqual(proof["production_image"], PROD_IMAGE)
                    self.assertEqual(proof["worker_version"], PROD_VERSION)
                    self.assertEqual(proof["production_deploy_id"], EVENT)
                    self.assertEqual(proof["receipt"]["build"], admitted_receipt()["build"])
                    for endpoint in ("health", "readyz"):
                        observations = {sample for sample, name in plane.runtime_calls
                                        if name == endpoint}
                        self.assertGreaterEqual(len(observations), 2)
                    self.assertTrue(clock.sleeps, "one live sample cannot establish a streak")
                    final_runtime = max(index for index, path in enumerate(plane.calls)
                                        if path.startswith(RUNTIME_URL + "/"))
                    worker_reads = [index for index, path in enumerate(plane.calls)
                                    if path == f"/workers/scripts/{PROD_WORKER}/deployments"]
                    self.assertGreater(max(worker_reads), final_runtime,
                                       "Worker identity must be re-read after runtime convergence")

    def test_final_cli_accepts_url_and_predeploy_snapshot(self):
        code, message, proof, _, _ = self.attempt(
            self.lag_row(), snapshot=self.snapshot(), cli=True)
        self.assertEqual(code, 0, message)
        self.assertEqual(proof["production_image"], PROD_IMAGE)

    def test_without_url_lag_never_replaces_strict_convergence(self):
        for active, healthy in ((0, 1), (1, 0)):
            with self.subTest(active=active, healthy=healthy):
                result = self.attempt(self.lag_row(active, healthy), url=None)
                self.assert_refused(result)
                self.assertEqual(result[3].runtime_calls, [])

    def test_failed_runtime_confirmation_resets_lag_streak(self):
        bad_samples = [
            {"readyz": runtime_answer(status=503)},
            {"readyz": runtime_answer(report={"components": [["gateway", "down"]]})},
            {"health": runtime_answer(status=503)},
        ]
        for bad in bad_samples:
            with self.subTest(bad=bad):
                code, message, proof, plane, _ = self.attempt(
                    self.lag_row(), samples=[{}, bad, {}, {}])
                self.assertEqual(code, 0, message)
                self.assertIsNotNone(proof)
                self.assertGreaterEqual(max(index for index, _ in plane.runtime_calls), 3,
                                        "pass, miss, pass must not certify; two fresh passes are required")

    def test_persistent_runtime_mismatches_cannot_certify_either_lag_shape(self):
        cases = {
            "readyz-503": {"readyz": runtime_answer(status=503)},
            "health-503": {"health": runtime_answer(status=503)},
            "parked-gateway": {"readyz": runtime_answer(report={"components": [["gateway", "down"]]})},
            "missing-gateway": {"readyz": runtime_answer(report={"components": []})},
            "other-component-only": {"readyz": runtime_answer(report={"components": [["db", "ready"]]})},
            "wrong-revision": {"readyz": runtime_answer(report={"build_revision": "f" * 40})},
            "production-build-id": {"readyz": runtime_answer(report={"build_id": "900-1"})},
            "production-event-id": {"readyz": runtime_answer(report={"deploy_id": EVENT})},
            "missing-build-stamp": {"readyz": runtime_answer(report={"build_id": None})},
            "wrong-readyz-worker": {"readyz": runtime_answer(version=OTHER_VERSION)},
            "wrong-health-worker": {"health": runtime_answer(version=OTHER_VERSION)},
            "missing-readyz-version": {"readyz": runtime_answer(version=None)},
            "missing-health-version": {"health": runtime_answer(version=None)},
            "duplicate-readyz-version": {"readyz": runtime_answer(duplicate=True)},
            "duplicate-health-version": {"health": runtime_answer(duplicate=True)},
            "transport-failure": {"readyz": base.GateError("runtime_transport_failure")},
        }
        for active, healthy in ((0, 1), (1, 0)):
            for name, sample in cases.items():
                with self.subTest(shape=(active, healthy), failure=name):
                    self.assert_refused(self.attempt(self.lag_row(active, healthy), samples=[sample]))

    def test_lag_does_not_relax_rollout_progress_or_instance_schema(self):
        mutations = [
            ("health", "instances", "active", True),
            ("health", "instances", "active", "0"),
            ("health", "instances", "healthy", -1),
            ("health", "instances", "failed", 1),
            ("health", "instances", "starting", 1),
            ("health", "instances", "scheduling", 1),
            ("health", "instances", "scheduling", None),
            ("progress", "updated_instances", 0),
            ("progress", "total_instances", 2),
            ("progress", "total_steps", 2),
            ("progress", "current_step", 2),
            ("steps", [{"status": "in_progress"}]),
            ("status", "progressing"),
            ("status", "replaced"),
            ("status", "failed"),
        ]
        for path in mutations:
            with self.subTest(path=path):
                row = self.lag_row()
                holder = row
                for key in path[:-2]:
                    holder = holder[key]
                holder[path[-2]] = path[-1]
                self.assert_refused(self.attempt(row))

    def test_current_application_image_and_version_must_match_even_with_live_readyz(self):
        for image, version in ((OLD_IMAGE, TARGET_VERSION), (PROD_IMAGE, TARGET_VERSION + 1)):
            with self.subTest(image=image, version=version):
                routes = self.live_routes(self.lag_row(), app_image=image, app_version=version)
                self.assert_refused(self.attempt(routes=routes))

    def test_worker_change_after_live_probes_refuses_the_old_version(self):
        routes = self.live_routes(self.lag_row())
        reads = [0]

        def deployment():
            reads[0] += 1
            return deployments_answer(PROD_VERSION if reads[0] < 3 else OTHER_VERSION)

        routes[f"/workers/scripts/{PROD_WORKER}/deployments"] = deployment
        self.assert_refused(self.attempt(routes=routes))

    def snapshot(self):
        return {"has_previous": True, "worker": PROD_WORKER, "application": PROD_APP,
                "application_id": APP_ID, "namespace_id": NAMESPACE,
                "serving_version": OTHER_VERSION, "serving_sha": "f" * 40,
                "provenance_complete": True, "serving_rollout_id": "roll-old",
                "serving_image": OLD_IMAGE, "recorded_at": 1000.0}

    def rollout_sequence(self, listings):
        reads = [0]
        desired = converged_row(ROLL_ID, PROD_IMAGE, created=NEWER_CREATED)
        old = converged_row("roll-old", OLD_IMAGE)
        routes = self.live_routes(desired)

        def listing():
            index = min(reads[0], len(listings) - 1)
            reads[0] += 1
            return copy.deepcopy(listings[index])

        routes[f"/containers/applications/{APP_ID}/rollouts?limit=100"] = listing
        routes[f"/containers/applications/{APP_ID}/rollouts/roll-old"] = lambda: copy.deepcopy(old)
        return routes, reads, desired, old

    def test_empty_rollout_list_waits_for_the_intended_rollout(self):
        desired = converged_row(ROLL_ID, PROD_IMAGE, created=NEWER_CREATED)
        routes, reads, _, _ = self.rollout_sequence([[], [desired]])
        code, message, proof, _, clock = self.attempt(routes=routes)
        self.assertEqual(code, 0, message)
        self.assertEqual(proof["rollout_id"], ROLL_ID)
        self.assertGreaterEqual(reads[0], 2)
        self.assertTrue(clock.sleeps)

    def test_only_snapshotted_old_rollout_can_poll_before_the_new_image(self):
        desired = converged_row(ROLL_ID, PROD_IMAGE, created=NEWER_CREATED)
        old = converged_row("roll-old", OLD_IMAGE)
        for preflight in (False, True):
            with self.subTest(preflight=preflight):
                routes, reads, _, _ = self.rollout_sequence([[], [old], [desired]])
                code, message, proof, _, clock = self.attempt(
                    routes=routes, snapshot=self.snapshot(), preflight=preflight,
                    url=None if preflight else RUNTIME_URL)
                self.assertEqual(code, 0, message)
                self.assertEqual(proof["rollout_id"], ROLL_ID)
                self.assertGreaterEqual(reads[0], 3)
                self.assertGreaterEqual(len(clock.sleeps), 2)

    def test_no_snapshot_does_not_authorize_waiting_on_a_foreign_image(self):
        old = converged_row("roll-old", OLD_IMAGE)
        routes, reads, _, _ = self.rollout_sequence([[old]])
        result = self.attempt(routes=routes)
        self.assert_refused(result)
        self.assertEqual(reads[0], 1)
        self.assertEqual(result[4].sleeps, [])

    def test_wrong_rollout_id_or_wrong_image_is_not_the_snapshotted_baseline(self):
        for snapshot in (dict(self.snapshot(), serving_rollout_id="different-old"),
                         dict(self.snapshot(), serving_image=PROD_REPO + "@sha256:" + "e" * 64)):
            with self.subTest(snapshot=snapshot):
                old = converged_row("roll-old", OLD_IMAGE)
                routes, reads, _, _ = self.rollout_sequence([[old]])
                result = self.attempt(routes=routes, snapshot=snapshot)
                self.assert_refused(result)
                self.assertEqual(reads[0], 1)
                self.assertEqual(result[4].sleeps, [])

    def test_newer_foreign_rollout_never_polls_even_when_old_baseline_is_known(self):
        old = converged_row("roll-old", OLD_IMAGE)
        foreign = converged_row("roll-foreign", OLD_IMAGE, created=NEWER_CREATED)
        routes, reads, _, _ = self.rollout_sequence([[old, foreign]])
        routes[f"/containers/applications/{APP_ID}/rollouts/roll-foreign"] = lambda: foreign
        result = self.attempt(routes=routes, snapshot=self.snapshot())
        self.assert_refused(result)
        self.assertEqual(reads[0], 1)
        self.assertEqual(result[4].sleeps, [])

    def test_first_deploy_snapshot_without_previous_rollout_remains_valid(self):
        snapshot = {"has_previous": False, "worker": PROD_WORKER, "application": PROD_APP}
        code, message, proof, _, _ = self.attempt(snapshot=snapshot)
        self.assertEqual(code, 0, message)
        self.assertEqual(proof["production_image"], PROD_IMAGE)

    def test_snapshot_without_previous_deployment_cannot_authorize_foreign_image_polling(self):
        snapshot = {"has_previous": False, "worker": PROD_WORKER, "application": PROD_APP}
        old = converged_row("roll-old", OLD_IMAGE)
        routes, reads, _, _ = self.rollout_sequence([[old]])
        result = self.attempt(routes=routes, snapshot=snapshot)
        self.assert_refused(result)
        self.assertEqual(reads[0], 1)
        self.assertEqual(result[4].sleeps, [])

    def test_snapshot_must_bind_the_worker_application_and_application_id(self):
        for key, value in (("worker", "two-bot-next-staging"),
                           ("application", "two-bot-next-twobotcontainer-staging"),
                           ("application_id", "app-foreign")):
            with self.subTest(key=key):
                snapshot = dict(self.snapshot(), **{key: value})
                self.assert_refused(self.attempt(snapshot=snapshot))

    def test_empty_or_only_old_listing_times_out_with_fixed_refusal(self):
        old = converged_row("roll-old", OLD_IMAGE)
        for listing in ([], [old]):
            with self.subTest(listing=listing):
                routes, reads, _, _ = self.rollout_sequence([listing])
                result = self.attempt(routes=routes, snapshot=self.snapshot())
                self.assert_refused(result)
                self.assertGreater(reads[0], 1)
                self.assertIn("identity refused", result[1])
                self.assertIn("image did not converge", result[1])


def header_file(root, name, version=PROD_VERSION):
    path = root / name
    path.write_bytes(b"HTTP/1.1 200 OK\r\nx-two-worker-version: "
                     + version.encode("ascii") + b"\r\n"
                     + b"content-type: application/json\r\n")
    return path


class ArtifactHeadersReadinessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def judge(self, body=None, status="200", headers=None, health=None,
              worker_version=PROD_VERSION, image=PROD_IMAGE,
              expected_version=PROD_VERSION, extra=()):
        artifact = self.root / "admitted.json"
        artifact.write_text(json.dumps(
            {"schema_version": 1, "receipt": admitted_receipt(),
             "production_image": PROD_IMAGE}))
        report = {"build_revision": SHA, "build_id": BUILD,
                  "deploy_id": STAMP, "components": [["gateway", "ready"]]}
        if body is not None:
            report.update(body)
        path = self.root / "readyz.json"
        path.write_text(json.dumps(report))
        argv = ["readyz", "--sha", SHA, "--mode", "deploy",
                "--build-id", "900-1", "--deploy-id", EVENT,
                "--status", status, "--body", str(path),
                "--artifact", str(artifact),
                "--expected-worker-version", expected_version,
                "--worker-version", worker_version, "--image", image]
        if headers is not None:
            argv += ["--headers", str(headers)]
        if health is not None:
            argv += ["--health-headers", str(health)]
        argv += list(extra)
        output = io.StringIO()
        with contextlib.redirect_stdout(output), \
                contextlib.redirect_stderr(output):
            try:
                code = production_deploy.main(argv)
            except SystemExit as exc:
                code = exc.code
        return code, output.getvalue()

    def live_headers(self):
        return (header_file(self.root, "readyz.headers"),
                header_file(self.root, "health.headers"))

    def test_actual_health_and_readyz_headers_with_original_stamps_pass(self):
        headers, health = self.live_headers()
        code, message = self.judge(headers=headers, health=health)
        self.assertEqual(code, 0, message)
        self.assertIn("gateway ready", message)

    def test_wrong_header_version_on_either_response_refuses(self):
        for role in ("readyz", "health"):
            with self.subTest(role=role):
                headers, health = self.live_headers()
                target = headers if role == "readyz" else health
                target.write_bytes(
                    b"HTTP/1.1 200 OK\r\nx-two-worker-version: "
                    + STAGE_VERSION.encode("ascii") + b"\r\n")
                code, message = self.judge(headers=headers, health=health)
                self.assertEqual(code, 1, message)
                self.assertIn("refused", message)

    def test_missing_duplicate_nonascii_or_oversize_header_refuses(self):
        variants = {
            "missing": b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n",
            "duplicate": (b"HTTP/1.1 200 OK\r\nx-two-worker-version: "
                          + PROD_VERSION.encode("ascii") + b"\r\n"
                          + b"x-two-worker-version: "
                          + PROD_VERSION.encode("ascii") + b"\r\n"),
            "non-ascii": (b"HTTP/1.1 200 OK\r\nx-two-worker-version: "
                          + PROD_VERSION.encode("ascii") + b"\xc3\xa9\r\n"),
            "oversize": (b"HTTP/1.1 200 OK\r\nx-two-worker-version: "
                         + PROD_VERSION.encode("ascii") + b"\r\n"
                         + b"#" * (64 * 1024)),
        }
        for role in ("readyz", "health"):
            for name, raw in variants.items():
                with self.subTest(role=role, variant=name):
                    headers, health = self.live_headers()
                    (headers if role == "readyz" else health).write_bytes(raw)
                    code, message = self.judge(headers=headers, health=health)
                    self.assertEqual(code, 1, message)
                    self.assertIn("refused", message)

    def test_this_run_ids_never_replace_original_admitted_stamps(self):
        headers, health = self.live_headers()
        code, message = self.judge(headers=headers, health=health)
        self.assertEqual(code, 0, message)
        for body in ({"build_id": "900-1"}, {"deploy_id": EVENT},
                     {"build_revision": "f" * 40}, {"deploy_id": "unknown"},
                     {"status": None}):
            with self.subTest(body=body):
                if "status" in body:
                    code, message = self.judge(
                        status="503", headers=headers, health=health)
                else:
                    code, message = self.judge(
                        body=body, headers=headers, health=health)
                self.assertEqual(code, 1, message)

    def test_parked_gateway_never_passes_with_headers(self):
        headers, health = self.live_headers()
        for body in ({"components": [["gateway", "down"]]},):
            with self.subTest(body=body):
                code, message = self.judge(
                    body=body, headers=headers, health=health)
                self.assertEqual(code, 1, message)
        code, message = self.judge(
            status="503", headers=headers, health=health)
        self.assertEqual(code, 1, message)


class LegacyReadinessPreservedTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def judge_legacy(self, raw=None, report=None, status="200"):
        path = self.root / "readyz.json"
        if raw is not None:
            path.write_bytes(raw)
        else:
            path.write_text(json.dumps(report))
        output = io.StringIO()
        with contextlib.redirect_stdout(output), \
                contextlib.redirect_stderr(output):
            try:
                code = production_deploy.main([
                    "readyz", "--sha", SHA, "--mode", "deploy",
                    "--build-id", "900-1", "--status", status,
                    "--body", str(path)])
            except SystemExit as exc:
                code = exc.code
        return code, output.getvalue()

    def live_report(self):
        return {"build_revision": SHA, "build_id": "900-1",
                "components": [["gateway", "ready"]]}

    def test_legacy_gateway_ready_pass_preserved(self):
        code, message = self.judge_legacy(report=self.live_report())
        self.assertEqual(code, 0, message)

    def test_legacy_body_cap_refuses_before_parsing(self):
        code, message = self.judge_legacy(
            raw=b'{"build_revision": "' + SHA.encode() + b'"}' + b"#" * (64 * 1024))
        self.assertNotEqual(code, 0, message)
        self.assertIn("body cap", message)

    def test_legacy_unparsable_or_mismatched_body_refuses(self):
        code, message = self.judge_legacy(raw=b"not json")
        self.assertNotEqual(code, 0, message)
        for report in (dict(self.live_report(), build_revision="f" * 40),
                       dict(self.live_report(), components=[["gateway", "down"]]),
                       {"build_revision": SHA}):
            with self.subTest(report=report):
                code, message = self.judge_legacy(report=report)
                self.assertNotEqual(code, 0, message)


if __name__ == "__main__":
    unittest.main()

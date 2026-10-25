"""Runtime-adapter acceptance for immutable production promotion.

Covers two seams of scripts/production_deploy.py without touching the
implementation:

* ``serving`` — GET-only control-plane proof that the production-local
  promoted image has converged under the exact current production Worker
  version and target application version, with the Worker re-read after
  image convergence. The proof binds the original admitted receipt and an
  independent canonical production deployment event.
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
from pathlib import Path
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

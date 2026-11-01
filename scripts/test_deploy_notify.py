"""Offline pins for scripts/deploy_notify.py (no network; a recording opener)."""

import contextlib
import hashlib
import hmac
import io
import json
import os
import sys
import tempfile
import unittest
import urllib.error
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first.
import deploy_notify as notify

SHA = "0123456789abcdef0123456789abcdef01234567"
VERSION = "11111111-1111-4111-8111-111111111111"
PREVIOUS = "22222222-2222-4222-8222-222222222222"
IMAGE = "registry.cloudflare.com/" + "a" * 32 + "/two-bot-next-twobotcontainer-production@sha256:" + "b" * 64
URL = "https://paperclip.example.test/api/routine-triggers/public/abc/fire"
SECRET = "s" * 40
RUN_URL = "https://github.com/TogetherWeOwn/two-bot-next/actions/runs/123"
NOW = 1_800_000_000


class Recorder:
    def __init__(self, status=202, error=None):
        self.requests, self.status, self.error = [], status, error

    def open(self, request, timeout):
        self.requests.append(request)
        if self.error is not None:
            raise self.error
        status = self.status

        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *_):
                return False
        response = Response()
        response.status = status
        return response


def run(argv, env=None, opener=None):
    opener = opener or Recorder()
    environ = {"PAPERCLIP_DEPLOY_WEBHOOK_URL": URL, "PAPERCLIP_DEPLOY_WEBHOOK_SECRET": SECRET,
               **(env or {})}
    stdout = io.StringIO()
    with contextlib.redirect_stdout(stdout):
        code = notify.main(argv, environ=environ, now=lambda: NOW, opener=opener)
    return code, opener, stdout.getvalue()


def sent(opener):
    request = opener.requests[-1]
    return request, json.loads(request.data)


class SendTests(unittest.TestCase):
    def test_signed_payload_matches_the_receiver_recipe(self):
        code, opener, stdout = run(["--event", "deploy", "--outcome", "success", "--sha", SHA,
                                    "--version", VERSION, "--image", IMAGE, "--previous-version", PREVIOUS,
                                    "--run-url", RUN_URL, "--started-at", "2027-01-15T08:00:00Z"])
        self.assertEqual(code, 0)
        request, body = sent(opener)
        self.assertEqual(request.full_url, URL)
        self.assertEqual(request.get_method(), "POST")
        timestamp = request.get_header("X-paperclip-timestamp")
        self.assertEqual(timestamp, str(NOW))
        expected = hmac.new(SECRET.encode(), f"{timestamp}.".encode() + request.data, hashlib.sha256).hexdigest()
        self.assertEqual(request.get_header("X-paperclip-signature"), f"sha256={expected}")
        self.assertEqual(body, {
            "repo": "two-bot-next", "event": "deploy", "outcome": "success", "sha": SHA,
            "versionId": VERSION, "imageDigest": IMAGE, "previousVersionId": PREVIOUS, "runUrl": RUN_URL,
            "startedAt": "2027-01-15T08:00:00Z", "finishedAt": "2027-01-15T08:00:00Z", "reason": None})
        self.assertNotIn(SECRET, stdout)
        self.assertNotIn(expected, stdout)

    def test_off_shape_values_are_dropped_never_forwarded(self):
        _, opener, _ = run(["--event", "rollback", "--outcome", "failed", "--sha", "abc",
                            "--version", "v1", "--image", IMAGE.replace("@sha256", ":latest"),
                            "--run-url", "https://evil.example/x", "--reason", "line\nbreak"])
        _, body = sent(opener)
        for key in ("sha", "versionId", "imageDigest", "runUrl", "reason"):
            self.assertIsNone(body[key], key)

    def test_unset_bindings_skip_without_any_request(self):
        for env in ({"PAPERCLIP_DEPLOY_WEBHOOK_URL": ""}, {"PAPERCLIP_DEPLOY_WEBHOOK_SECRET": ""}):
            with self.subTest(env=env):
                code, opener, stdout = run(["--event", "deploy", "--outcome", "success"], env=env)
                self.assertEqual(code, 0)
                self.assertEqual(opener.requests, [])
                self.assertIn("deploy notify skipped", stdout)

    def test_non_https_or_credentialed_urls_are_refused(self):
        for url in ("http://paperclip.example.test/fire", "https://user:pw@paperclip.example.test/fire"):
            with self.subTest(url=url):
                code, opener, _ = run(["--event", "deploy", "--outcome", "success"],
                                      env={"PAPERCLIP_DEPLOY_WEBHOOK_URL": url})
                self.assertEqual((code, opener.requests), (1, []))

    def test_rejected_or_unreachable_delivery_warns_and_exits_nonzero(self):
        for opener in (Recorder(status=401),
                       Recorder(error=urllib.error.HTTPError(URL, 503, "x", {}, None)),
                       Recorder(error=OSError("down"))):
            with self.subTest(opener=opener):
                code, _, stdout = run(["--event", "deploy", "--outcome", "success"], opener=opener)
                self.assertEqual(code, 1)
                self.assertIn("::warning::deploy notify was not accepted", stdout)


class FromWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.temp = Path(self.tmp.name)
        (self.temp / "production-rollback-snapshot.json").write_text(json.dumps(
            {"has_previous": True, "serving_version": PREVIOUS}))
        (self.temp / "production-promotion-evidence.json").write_text(json.dumps(
            {"production_image": IMAGE}))

    def tearDown(self):
        self.tmp.cleanup()

    def derive(self, **env):
        base = {"RUNNER_TEMP": str(self.temp), "MODE": "deploy", "NEW_VERSION": VERSION}
        return notify.from_workflow({**base, **env})

    def test_a_clean_promotion_reports_success_with_both_versions_and_the_image(self):
        self.assertEqual(self.derive(JOB_STATUS="success"),
                         ("deploy", "success", VERSION, IMAGE, PREVIOUS, ""))

    def test_an_auto_rollback_names_the_first_failed_gate(self):
        event, outcome, _, _, _, reason = self.derive(
            JOB_STATUS="failure", GATE_OUTCOME="success", PROMOTION_OUTCOME="success",
            ERROR_GATE_OUTCOME="failure", AUTO_ROLLBACK_OUTCOME="success")
        self.assertEqual((event, outcome), ("deploy", "rolled_back"))
        self.assertEqual(reason, "auto-rollback after the REST 5xx error-rate gate failed")
        self.assertEqual(self.derive(JOB_STATUS="failure", GATE_OUTCOME="failure",
                                     AUTO_ROLLBACK_OUTCOME="success")[5],
                         "auto-rollback after the readyz gate failed")

    def test_a_failure_without_rollback_reports_failed(self):
        event, outcome, *_, reason = self.derive(JOB_STATUS="failure", AUTO_ROLLBACK_OUTCOME="skipped",
                                                 GATE_OUTCOME="failure")
        self.assertEqual((event, outcome), ("deploy", "failed"))
        self.assertIn("readyz gate failed; no automatic rollback ran", reason)

    def test_a_failed_auto_rollback_reports_the_failed_restore(self):
        event, outcome, *_, reason = self.derive(JOB_STATUS="failure", AUTO_ROLLBACK_OUTCOME="failure",
                                                 GATE_OUTCOME="failure")
        self.assertEqual((event, outcome), ("deploy", "failed"))
        self.assertIn("readyz gate failed; the automatic rollback also failed", reason)

    def test_a_live_sha_recovery_names_its_mode(self):
        # A recovery builds a fresh image at deploy time, so no promotion
        # evidence exists and the digest stays null; the reason still tells
        # the release log which mode produced the record.
        (self.temp / "production-promotion-evidence.json").unlink()
        derived = self.derive(JOB_STATUS="success", ALLOW_UNSTAGED="true")
        self.assertEqual(derived, ("deploy", "success", VERSION, "", PREVIOUS, "live-SHA recovery"))

    def test_a_watch_rollback_reports_its_breach(self):
        derived = self.derive(JOB_STATUS="failure", GATE_OUTCOME="success", AUTO_ROLLBACK_OUTCOME="skipped",
                              WATCH_ROLLBACK_OUTCOME="success", WATCH_REASON="readiness:health_not_200")
        self.assertEqual(derived[:2], ("deploy", "rolled_back"))
        self.assertEqual(derived[5], "auto-rollback after the post-deploy watch breached (readiness:health_not_200)")

    def test_every_generated_reason_survives_the_allowlist(self):
        # A reason the allowlist drops would reach the release log as null.
        cases = [
            {"MODE": "rollback", "ROLLBACK_OUTCOME": "failure", "JOB_STATUS": "failure"},
            {"MODE": "rollback", "ROLLBACK_OUTCOME": "success", "JOB_STATUS": "failure", "GATE_OUTCOME": "failure"},
            {"MODE": "rollback", "ROLLBACK_OUTCOME": "success", "JOB_STATUS": "failure", "GATE_OUTCOME": "success"},
            {"JOB_STATUS": "success", "ALLOW_UNSTAGED": "true"},
            {"JOB_STATUS": "failure", "GATE_OUTCOME": "failure"},
            {"JOB_STATUS": "failure"},
            {"JOB_STATUS": "failure", "GATE_OUTCOME": "failure", "AUTO_ROLLBACK_OUTCOME": "failure"},
            {"JOB_STATUS": "failure", "ERROR_GATE_OUTCOME": "failure", "AUTO_ROLLBACK_OUTCOME": "success"},
            {"JOB_STATUS": "failure", "WATCH_ROLLBACK_OUTCOME": "success", "WATCH_REASON": "rest_5xx"},
        ]
        for env in cases:
            with self.subTest(env=env):
                derived = self.derive(**env)
                code, opener, _ = run(["--from-workflow", "--sha", SHA],
                                      env={"RUNNER_TEMP": str(self.temp), "NEW_VERSION": VERSION,
                                           "MODE": "deploy", **env})
                self.assertEqual(code, 0)
                self.assertEqual(sent(opener)[1]["reason"], derived[5])
                self.assertIsNotNone(sent(opener)[1]["reason"])

    def test_an_operator_rollback_reports_its_target(self):
        derived = self.derive(MODE="rollback", ROLLBACK_OUTCOME="success", GATE_OUTCOME="success",
                              JOB_STATUS="success", TARGET_VERSION=PREVIOUS, TARGET_IMAGE=IMAGE)
        self.assertEqual(derived, ("rollback", "success", PREVIOUS, IMAGE, "", "operator rollback dispatch"))
        self.assertEqual(self.derive(MODE="rollback", ROLLBACK_OUTCOME="failure", JOB_STATUS="failure")[1],
                         "failed")

    def test_a_restored_rollback_with_failed_bookkeeping_reports_the_restore(self):
        # The deployment record (or a later probe) can fail red after the
        # restore verified: the release log must still say production was
        # restored, with the bookkeeping failure in the reason.
        derived = self.derive(MODE="rollback", ROLLBACK_OUTCOME="success", GATE_OUTCOME="success",
                              JOB_STATUS="failure", TARGET_VERSION=PREVIOUS, TARGET_IMAGE=IMAGE)
        self.assertEqual(derived, ("rollback", "success", PREVIOUS, IMAGE, "",
                                   "operator rollback dispatch; restored, but a later step failed"))

    def test_an_unverified_rollback_reports_failed(self):
        # The rollback step succeeded but the revision gate never passed, so
        # the restore is unverified: report the failure, not the restore.
        derived = self.derive(MODE="rollback", ROLLBACK_OUTCOME="success", GATE_OUTCOME="failure",
                              JOB_STATUS="failure", TARGET_VERSION=PREVIOUS, TARGET_IMAGE=IMAGE)
        self.assertEqual(derived[:2], ("rollback", "failed"))
        self.assertIn("failed at readyz gate", derived[5])

    def test_missing_receipts_report_no_ids_instead_of_failing(self):
        for name in ("production-rollback-snapshot.json", "production-promotion-evidence.json"):
            (self.temp / name).write_text("not json")
        self.assertEqual(self.derive(JOB_STATUS="success")[2:5], (VERSION, "", ""))

    def test_main_from_workflow_sends_the_derived_record(self):
        code, opener, _ = run(["--from-workflow", "--sha", SHA, "--run-url", RUN_URL,
                               "--started-at", "2027-01-15T07:00:00Z"],
                              env={"RUNNER_TEMP": str(self.temp), "MODE": "deploy", "NEW_VERSION": VERSION,
                                   "JOB_STATUS": "success"})
        self.assertEqual(code, 0)
        _, body = sent(opener)
        self.assertEqual((body["event"], body["outcome"], body["versionId"], body["previousVersionId"]),
                         ("deploy", "success", VERSION, PREVIOUS))

    def test_explicit_mode_needs_event_and_outcome(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            notify.main(["--event", "deploy"], environ={})


if __name__ == "__main__":
    unittest.main()

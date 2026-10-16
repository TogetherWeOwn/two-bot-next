"""Offline synthetic-alert fixtures; stdlib only, no network access."""

import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
alert = importlib.import_module("synthetic_alert")
history = importlib.import_module("synthetic_alert_history")

HOOK = "https://discord.com/api/webhooks/123456789012345678/synthetic-token-not-a-secret"

EVIDENCE = {
    "origin": "https://two-bot-next-staging.example.workers.dev",
    "utc": "2026-10-16T00:00:00Z",
    "checks": [
        {"check": "health", "verdict": "pass", "shape": "health 200"},
        {"check": "readyz", "verdict": "fail", "shape": "readyz 503: parked"},
        {"check": "build", "verdict": "pass", "shape": "build_revision abc build_id 1"},
    ],
    "build_revision": "abc123",
    "build_id": "build-1",
    "result": "fail",
}


class SyntheticAlertTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(alert.urllib.request, "urlopen",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.evidence = str(self.dir / "evidence.json")
        Path(self.evidence).write_text(json.dumps(EVIDENCE))

    def base_env(self):
        return {"SYNTHETIC_ALERT_WEBHOOK_URL": HOOK,
                "RUN_ID": "999", "GITHUB_RUN_ID": "999", "GITHUB_TOKEN": "test-token",
                "PROBE_OUTCOME": "failure", "FORCE_FAIL": "true",
                "GIT_SHA": "deadbeef",
                "GITHUB_SERVER_URL": "https://github.com",
                "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next"}

    def test_content_reuses_alert_pattern_with_build_and_log_link(self):
        content = alert.build_content("staging", EVIDENCE, "deadbeef",
                                      "https://github.com/TogetherWeOwn/two-bot-next/actions/runs/999")
        self.assertTrue(content.startswith("two-bot-next ALERT synthetic-user-flow:staging: "))
        self.assertIn("readyz", content)
        self.assertIn("Runbook: " + alert.RUNBOOK_URL, content)
        self.assertIn("build abc123 build-1 (git deadbeef)", content)
        self.assertIn("logs https://github.com/TogetherWeOwn/two-bot-next/actions/runs/999", content)
        self.assertLessEqual(len(content), alert.MAX_CONTENT)

    def test_missing_evidence_reads_as_unknown_build(self):
        content = alert.build_content("prod", None, None, "link")
        self.assertIn("synthetic-user-flow:prod", content)
        self.assertIn("build unknown unknown (git unknown)", content)

    def test_alert_transition_requires_two_failures_and_recovers_once(self):
        self.assertIsNone(alert.transition("failure", []))
        self.assertEqual(alert.transition("failure", ["failure"]), "alert")
        self.assertIsNone(alert.transition("failure", ["failure", "failure"]))
        self.assertEqual(alert.transition("success", ["failure", "failure"]), "recovery")
        self.assertIsNone(alert.transition("success", ["success", "failure"]))
        self.assertEqual(alert.transition("failure", [], force_fail=True), "alert")
        recovery = alert.build_content("staging", EVIDENCE, "deadbeef", "link", "recovery")
        self.assertTrue(recovery.startswith("two-bot-next RECOVERY synthetic-user-flow:staging"))
        self.assertIn("healthy again", recovery)

    def test_history_uses_only_prior_scheduled_probe_samples(self):
        runs = {"workflow_runs": [
            {"id": 102, "run_number": 11, "event": "workflow_dispatch"},
            {"id": 101, "run_number": 10, "event": "schedule"},
            {"id": 100, "run_number": 9, "event": "schedule"},
        ]}
        jobs = {"jobs": [{
            "name": "staging user-flow synthetic",
            "steps": [{"name": history.PROBE_STEPS["staging"], "conclusion": "failure"}],
        }]}
        with mock.patch.object(history, "_get", side_effect=[runs, jobs]) as get:
            outcomes = history.previous_probe_outcomes(
                {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
                 "GITHUB_RUN_ID": "101"}, "staging")
        self.assertEqual(outcomes, ["failure"])
        self.assertEqual(get.call_count, 2)
        self.assertIn("event=schedule", get.call_args_list[0].args[0])
        self.assertIn("/runs/100/jobs?", get.call_args_list[1].args[0])

    def test_non_discord_destination_refuses_before_any_request(self):
        with self.assertRaisesRegex(alert.AlertError, "not a Discord webhook"):
            alert.post("https://example.com/hook", "content")

    def test_http_error_reports_status_only(self):
        import urllib.error
        with mock.patch.object(alert.urllib.request, "urlopen",
                               side_effect=urllib.error.HTTPError(
                                   HOOK, 400, "bad", {}, None)):
            with self.assertRaisesRegex(alert.AlertError, "status 400"):
                alert.post(HOOK, "content")

    def test_missing_webhook_secret_fails_without_a_request(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, {"PROBE_OUTCOME": "failure"}, clear=True), \
                redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes", return_value=["failure"]), \
                mock.patch.object(alert.urllib.request, "urlopen") as opened:
            code = alert.main(["--env", "staging", "--evidence", self.evidence])
        self.assertEqual(code, 1)
        opened.assert_not_called()
        self.assertIn("not set", err.getvalue())

    def test_delivers_once_with_allowed_mentions_off(self):
        seen = {}

        class FakeResponse:
            status = 204

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, _n):
                return b""

        def fake(request, timeout=None):
            seen["url"] = request.full_url
            seen["body"] = json.loads(request.data.decode())
            seen["timeout"] = timeout
            return FakeResponse()

        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, self.base_env(), clear=True), \
                redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes", return_value=[]), \
                mock.patch.object(alert.urllib.request, "urlopen", side_effect=fake):
            code = alert.main(["--env", "staging", "--evidence", self.evidence])
        self.assertEqual(code, 0)
        self.assertEqual(seen["url"], HOOK)
        self.assertIn("two-bot-next ALERT", seen["body"]["content"])
        self.assertEqual(seen["body"]["allowed_mentions"], {"parse": [], "replied_user": False})
        self.assertNotIn(HOOK, out.getvalue() + err.getvalue())

    def test_non_2xx_fails(self):
        class FakeResponse:
            status = 500

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, _n):
                return b""

        err = io.StringIO()
        with mock.patch.dict(os.environ, self.base_env(), clear=True), \
                redirect_stdout(io.StringIO()), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes", return_value=[]), \
                mock.patch.object(alert.urllib.request, "urlopen",
                                  return_value=FakeResponse()):
            code = alert.main(["--env", "prod", "--evidence", self.evidence])
        self.assertEqual(code, 1)
        self.assertIn("status 500", err.getvalue())


if __name__ == "__main__":
    unittest.main()

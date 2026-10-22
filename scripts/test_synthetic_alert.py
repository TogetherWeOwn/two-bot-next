"""Offline synthetic-alert fixtures; stdlib only, no network access."""

import importlib
import io
import json
import os
import sys
import tempfile
import unittest
import urllib.request
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

# Captured before any test patches urlopen, for the real-_get fixtures.
_REAL_URLOPEN = urllib.request.urlopen

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

    def test_alert_transition_retries_failed_delivery_and_recovers_once(self):
        self.assertIsNone(alert.transition("failure", []))
        self.assertEqual(alert.transition("failure", [("failure", "success")]), "alert")
        delivered = [("failure", "success"), ("failure", "success")]
        self.assertIsNone(alert.transition("failure", delivered))
        self.assertEqual(alert.transition("success", delivered), "recovery")
        self.assertIsNone(alert.transition("success", [("success", "success"), *delivered[:1]]))
        undelivered = [("failure", "failure"), ("failure", "success")]
        self.assertEqual(alert.transition("failure", undelivered), "alert")
        self.assertEqual(alert.transition("success", undelivered), "alert")
        self.assertEqual(alert.transition("failure", [], force_fail=True), "alert")
        recovery = alert.build_content("staging", EVIDENCE, "deadbeef", "link", "recovery")
        self.assertTrue(recovery.startswith("two-bot-next RECOVERY synthetic-user-flow:staging"))
        self.assertIn("healthy again", recovery)

    def test_skipped_noop_never_closes_incident_or_fakes_delivery(self):
        # Post-split history: success means delivered, failure means
        # undelivered, skipped/None is a no-op that leaves state unchanged.
        # [(failure, success), (failure, success), (success, skipped)] keeps
        # the incident open with recovery still due on the next green run.
        previous = [("success", "skipped"), ("failure", "success"), ("failure", "success")]
        failures, incident, delivered = history.incident_state(previous)
        self.assertTrue(incident)
        self.assertTrue(delivered)
        self.assertEqual(history.transition("success", previous), "recovery")
        # Mirror image: a failed post followed by a skipped green no-op must
        # not read as delivered, so recovery is withheld and the late alert
        # is still due.
        undelivered = [("success", "skipped"), ("failure", "failure"), ("failure", "success")]
        failures, incident, delivered = history.incident_state(undelivered)
        self.assertTrue(incident)
        self.assertFalse(delivered)
        self.assertEqual(history.transition("success", undelivered), "alert")

    def test_history_error_on_green_keeps_incident_open(self):
        # A history-read error on the first green run after an incident must
        # not close the incident: decide returns no event (green stays green)
        # and the next green run with good history still owes the recovery.
        with mock.patch.object(alert, "previous_probe_outcomes",
                               side_effect=history.HistoryError("history unavailable")):
            self.assertIsNone(alert.decide_event({**self.base_env(), "PROBE_OUTCOME": "success",
                                                  "FORCE_FAIL": "false"}, "staging"))
        delivered = [("failure", "success"), ("failure", "success")]
        self.assertEqual(alert.transition("success", delivered), "recovery")

    def test_decide_only_writes_event_and_post_only_delivers_without_history(self):
        import tempfile
        with tempfile.NamedTemporaryFile(mode="w+", delete=False) as output:
            output_path = output.name
        try:
            with mock.patch.dict(os.environ, {**self.base_env(), "PROBE_OUTCOME": "failure",
                                                "FORCE_FAIL": "false",
                                                "GITHUB_OUTPUT": output_path}, clear=True), \
                    redirect_stderr(io.StringIO()), \
                    mock.patch.object(alert, "previous_probe_outcomes",
                                      return_value=[("failure", "success")]):
                code = alert.main(["--env", "staging", "--evidence", self.evidence,
                                   "--decide-only"])
            self.assertEqual(code, 0)
            self.assertIn("event=alert", Path(output_path).read_text())
            with mock.patch.dict(os.environ, {**self.base_env(), "ALERT_EVENT": "alert"},
                                 clear=True), \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()), \
                    mock.patch.object(alert, "post") as post, \
                    mock.patch.object(alert, "previous_probe_outcomes",
                                      side_effect=AssertionError("post must not read history")):
                code = alert.main(["--env", "staging", "--evidence", self.evidence,
                                   "--post-only"])
            self.assertEqual(code, 0)
            post.assert_called_once()
        finally:
            Path(output_path).unlink(missing_ok=True)

    def test_history_uses_only_prior_scheduled_probe_samples(self):
        runs = {"workflow_runs": [
            {"id": 102, "run_number": 11, "event": "workflow_dispatch"},
            {"id": 101, "run_number": 10, "event": "schedule"},
            {"id": 100, "run_number": 9, "event": "schedule"},
        ]}
        jobs = {"jobs": [{
            "name": history.JOB_NAMES["staging"],
            "steps": [
                {"name": history.PROBE_STEPS["staging"], "conclusion": "failure"},
                {"name": history.ALERT_STEPS["staging"], "conclusion": "failure"},
            ],
        }]}
        with mock.patch.object(history, "_get", side_effect=[runs, jobs]) as get:
            outcomes = history.previous_probe_outcomes(
                {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
                 "GITHUB_RUN_ID": "101"}, "staging")
        self.assertEqual(outcomes, [("failure", "failure")])
        self.assertEqual(get.call_count, 2)
        self.assertIn("event=schedule", get.call_args_list[0].args[0])
        self.assertIn("/runs/100/jobs?", get.call_args_list[1].args[0])

    def test_carried_delivery_does_not_leak_across_a_healthy_sample(self):
        # Newest-first: a sub-threshold delivered failure, a healthy sample,
        # then a fresh two-failure streak with no delivery. The healthy
        # sample ends the first streak, so the new incident is undelivered.
        previous = [("failure", "skipped"), ("failure", "skipped"),
                    ("success", "skipped"), ("failure", "success")]
        failures, incident, delivered = history.incident_state(previous)
        self.assertTrue(incident)
        self.assertFalse(delivered)
        self.assertEqual(history.transition("failure", previous), "alert")

    def test_reconstruction_bound_reports_unknown_history(self):
        # Endless failures whose post step never ran never checkpoint (the
        # delivered state depends on older history), so the bounded scan
        # must report unknown history instead of returning a partial suffix.
        import urllib.parse

        def fake_get(path, _token):
            query = urllib.parse.parse_qs(urllib.parse.urlparse(path).query)
            if "/actions/workflows/" in path:
                page = int(query.get("page", ["1"])[0])
                first = 1000 - (page - 1) * 2
                return {"total_count": 9999, "workflow_runs": [
                    {"id": first - offset, "run_number": first - offset,
                     "event": "schedule"} for offset in range(2)]}
            return {"jobs": [{
                "name": history.JOB_NAMES["staging"],
                "steps": [
                    {"name": history.PROBE_STEPS["staging"], "conclusion": "failure"},
                    {"name": history.ALERT_STEPS["staging"], "conclusion": "skipped"},
                ]}]}

        with mock.patch.object(history, "MAX_HISTORY_RUNS", 5), \
                mock.patch.object(history, "RUNS_PAGE_SIZE", 2), \
                mock.patch.object(history, "_get", side_effect=fake_get):
            with self.assertRaisesRegex(history.HistoryError, "unknown"):
                history.previous_probe_outcomes(
                    {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
                     "GITHUB_RUN_ID": "2000"}, "staging")

    def fake_history(self, samples, decide="success", job_conclusions=None):
        """Paginated fake for previous_probe_outcomes over chronological samples.

        Each job carries the conclusion a normally finished job would have
        ("failure" when the probe or decide step failed, else "success")
        unless `job_conclusions` overrides it per chronological index.
        """
        import re
        import urllib.parse

        class Fake:
            def __init__(self):
                self.samples = list(samples)
                self.jobs_calls = 0

            def __call__(self, path, _token):
                parsed = urllib.parse.urlparse(path)
                query = urllib.parse.parse_qs(parsed.query)
                if "/actions/workflows/" in parsed.path:
                    runs = [{"id": 10000 + i, "run_number": i + 1, "event": "schedule"}
                            for i in range(len(self.samples))]
                    runs.append({"id": 99999, "run_number": len(runs) + 1,
                                 "event": "schedule"})
                    newest = sorted(runs, key=lambda run: run["run_number"], reverse=True)
                    page = int(query.get("page", ["1"])[0])
                    per_page = min(int(query.get("per_page", ["30"])[0]), 100)
                    start = (page - 1) * per_page
                    return {"total_count": len(newest),
                            "workflow_runs": newest[start:start + per_page]}
                self.jobs_calls += 1
                index = int(re.search(r"/actions/runs/(\d+)/jobs", parsed.path).group(1)) - 10000
                probe, delivery = self.samples[index]
                steps = [{"name": history.PROBE_STEPS["staging"], "conclusion": probe}]
                if decide is not None:
                    steps.append({"name": history.DECIDE_STEPS["staging"],
                                  "conclusion": decide})
                steps.append({"name": history.ALERT_STEPS["staging"],
                              "conclusion": delivery})
                conclusion = (job_conclusions or {}).get(
                    index,
                    "failure" if probe == "failure" or decide == "failure"
                    else "success")
                return {"jobs": [{"name": history.JOB_NAMES["staging"],
                                  "conclusion": conclusion, "steps": steps}]}

            def env(self, current):
                return {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
                        "GITHUB_RUN_ID": "99999", "PROBE_OUTCOME": current,
                        "FORCE_FAIL": "false"}

        return Fake()

    def test_long_quiet_history_keeps_debounce_and_stops_early(self):
        # 250 healthy prior runs (past the 200-run bound) then S,F,F,F,F,S,S:
        # a known-closed healthy sample checkpoints reconstruction, so the
        # two-failure debounce survives a quiet day and each decision reads
        # only a few jobs instead of scanning to the bound.
        fake = self.fake_history([("success", "skipped")] * 250)
        events, worst_jobs = [], 0
        for current in ("success", "failure", "failure", "failure",
                        "failure", "success", "success"):
            fake.jobs_calls = 0
            with mock.patch.object(history, "_get", side_effect=fake):
                event = alert.decide_event(fake.env(current), "staging")
            events.append(event)
            worst_jobs = max(worst_jobs, fake.jobs_calls)
            fake.samples.append((current, "success" if event else "skipped"))
        self.assertEqual(events, [None, None, "alert", None, None, "recovery", None])
        self.assertLess(worst_jobs, 10,
                        f"steady state read {worst_jobs} jobs in one decision")

    def test_known_closed_sample_closes_state_and_raw_skipped_stays_ambiguous(self):
        # A ("success", "closed") sample is the explicit checkpoint record;
        # a raw ("success", "skipped") sample with no decide record still
        # leaves an open incident open (test_skipped_noop_* above).
        closed = [("success", "closed"), ("failure", "skipped"), ("failure", "success")]
        self.assertEqual(history.incident_state(closed), (0, False, False))
        fake = self.fake_history([("success", "skipped")] * 250)
        with mock.patch.object(history, "_get", side_effect=fake):
            outcomes = history.previous_probe_outcomes(fake.env("failure"), "staging")
        self.assertTrue(outcomes)
        self.assertTrue(all(sample == ("success", "closed") for sample in outcomes))
        self.assertLessEqual(fake.jobs_calls, 3)

    def test_failed_decide_step_keeps_healthy_sample_ambiguous(self):
        # History was unknown on the healthy run after a delivered outage
        # (decide step failed to record it): recovery is still owed.
        samples = [("failure", "skipped"), ("failure", "success"),
                   ("failure", "skipped"), ("success", "skipped")]
        fake = self.fake_history(samples, decide="failure")
        with mock.patch.object(history, "_get", side_effect=fake):
            outcomes = history.previous_probe_outcomes(fake.env("success"), "staging")
        self.assertIn(("success", "skipped"), outcomes)
        self.assertNotIn(("success", "closed"), outcomes)
        with mock.patch.object(history, "_get", side_effect=fake):
            self.assertEqual(alert.decide_event(fake.env("success"), "staging"), "recovery")

    def test_bound_over_all_healthy_skipped_settles_closed_not_unknown(self):
        # More healthy no-post samples than the bound, none with a decide
        # record: settle closed instead of latching into HistoryError.
        fake = self.fake_history([("success", "skipped")] * 20, decide=None)
        with mock.patch.object(history, "MAX_HISTORY_RUNS", 5), \
                mock.patch.object(history, "_get", side_effect=fake):
            outcomes = history.previous_probe_outcomes(fake.env("failure"), "staging")
        self.assertIsNone(history.transition("failure", outcomes),
                          "a single failure after a long quiet stretch must not alert")
        # But a post step that ran on a healthy sample is incident
        # evidence: the same over-bound shape still reports unknown.
        # (Repeated failed posts never converge: an open undelivered
        # incident and a closed state replay them differently forever.)
        fake = self.fake_history([("success", "failure")] * 20, decide=None)
        with mock.patch.object(history, "MAX_HISTORY_RUNS", 5), \
                mock.patch.object(history, "_get", side_effect=fake):
            with self.assertRaisesRegex(history.HistoryError, "unknown"):
                history.previous_probe_outcomes(fake.env("failure"), "staging")

    def test_decide_only_fails_step_only_for_unknown_healthy(self):
        import tempfile
        cases = (("success", history.HistoryError("history unavailable"), 1, "event=\n"),
                 ("failure", history.HistoryError("history unavailable"), 0, "event=alert\n"),
                 ("success", [("success", "closed")], 0, "event=\n"))
        for outcome, previous, expected_code, expected_line in cases:
            with self.subTest(outcome=outcome, expected_code=expected_code):
                with tempfile.NamedTemporaryFile(mode="w+", delete=False) as output:
                    output_path = output.name
                try:
                    env = {**self.base_env(), "PROBE_OUTCOME": outcome,
                           "FORCE_FAIL": "false", "GITHUB_OUTPUT": output_path}
                    with mock.patch.dict(os.environ, env, clear=True), \
                            redirect_stderr(io.StringIO()), \
                            mock.patch.object(alert, "previous_probe_outcomes",
                                              side_effect=previous
                                              if isinstance(previous, Exception)
                                              else None,
                                              return_value=previous):
                        code = alert.main(["--env", "staging", "--evidence", self.evidence,
                                           "--decide-only"])
                    self.assertEqual(code, expected_code)
                    self.assertEqual(Path(output_path).read_text(), expected_line)
                finally:
                    Path(output_path).unlink(missing_ok=True)

    def test_long_outage_stops_at_decided_none_checkpoint_with_few_jobs_reads(self):
        # A 130-failure delivered outage: after the opening alert every
        # failed run decided no event (decide step succeeded, post step
        # skipped, job finished normally). Two such records in a row prove
        # the incident is open and delivered, so reconstruction stops after
        # a few jobs reads instead of re-reading the whole outage (which at
        # the five-minute schedule would exhaust the GITHUB_TOKEN budget).
        samples = [("success", "skipped"), ("failure", "skipped"),
                   ("failure", "success")] + [("failure", "skipped")] * 128
        fake = self.fake_history(samples)
        with mock.patch.object(history, "_get", side_effect=fake):
            self.assertIsNone(alert.decide_event(fake.env("failure"), "staging"))
        self.assertLess(fake.jobs_calls, 10,
                        f"one decision re-read {fake.jobs_calls} jobs mid-outage")
        fake.jobs_calls = 0
        with mock.patch.object(history, "_get", side_effect=fake):
            self.assertEqual(alert.decide_event(fake.env("success"), "staging"),
                             "recovery")
        self.assertLess(fake.jobs_calls, 10,
                        f"one decision re-read {fake.jobs_calls} jobs mid-outage")

    def test_cancelled_job_cannot_record_a_closed_checkpoint(self):
        # The healthy run after a delivered outage decided recovery, then
        # its job was cancelled before the post step ran (!cancelled gate):
        # the same probe/decide/post conclusions as a known-closed sample,
        # but the job did not finish normally. The recovery stays owed.
        samples = [("failure", "skipped"), ("failure", "success"),
                   ("failure", "skipped"), ("success", "skipped")]
        fake = self.fake_history(samples, job_conclusions={3: "cancelled"})
        with mock.patch.object(history, "_get", side_effect=fake):
            outcomes = history.previous_probe_outcomes(fake.env("success"), "staging")
        self.assertNotIn(("success", "closed"), outcomes)
        with mock.patch.object(history, "_get", side_effect=fake):
            self.assertEqual(alert.decide_event(fake.env("success"), "staging"),
                             "recovery")

    def test_cancelled_job_cannot_record_a_decided_none_checkpoint(self):
        # Failed runs whose jobs were cancelled after the decide step leave
        # a skipped post step that must not prove the alert was delivered:
        # an outage nobody was told about keeps alerting.
        samples = [("failure", "skipped")] * 6
        fake = self.fake_history(samples,
                                  job_conclusions={i: "cancelled" for i in range(6)})
        with mock.patch.object(history, "_get", side_effect=fake):
            self.assertEqual(alert.decide_event(fake.env("failure"), "staging"),
                             "alert")

    def serve_history(self, runs_body, jobs_body):
        """Serve runs/jobs JSON over loopback HTTP for the real _get."""
        import http.server
        import threading
        import urllib.parse

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                parsed = urllib.parse.urlparse(self.path)
                body = jobs_body if "/jobs" in parsed.path else runs_body(
                    urllib.parse.parse_qs(parsed.query))
                payload = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def stop():
            server.shutdown()
            server.server_close()

        self.addCleanup(stop)
        return f"http://127.0.0.1:{server.server_port}"

    def test_runs_pages_stay_under_response_cap_through_real_get(self):
        # Real Actions run objects are 15-40 KB (head_commit plus two
        # repository objects), so a 100-run page is ~2 MB and would be
        # truncated by the 1 MiB response cap in _get. Serve realistic-size
        # pages through the real _get and require the scan to complete.
        pad = "x" * 20_000
        runs = [{"id": 10000 + i, "run_number": i + 1, "event": "schedule",
                 "head_commit": {"message": pad}} for i in range(100)]
        runs.append({"id": 99999, "run_number": 101, "event": "schedule",
                     "head_commit": {"message": pad}})

        def runs_body(query):
            page = int(query.get("page", ["1"])[0])
            per_page = min(int(query.get("per_page", ["30"])[0]), 100)
            newest = sorted(runs, key=lambda run: run["run_number"], reverse=True)
            start = (page - 1) * per_page
            return {"total_count": len(newest),
                    "workflow_runs": newest[start:start + per_page]}

        jobs_body = {"jobs": [{
            "name": history.JOB_NAMES["staging"], "conclusion": "success",
            "steps": [
                {"name": history.PROBE_STEPS["staging"], "conclusion": "success"},
                {"name": history.DECIDE_STEPS["staging"], "conclusion": "success"},
                {"name": history.ALERT_STEPS["staging"], "conclusion": "skipped"},
            ]}]}
        base = self.serve_history(runs_body, jobs_body)
        env = {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
               "GITHUB_RUN_ID": "99999"}
        with mock.patch.object(history, "API", base), \
                mock.patch.object(urllib.request, "urlopen", _REAL_URLOPEN):
            outcomes = history.previous_probe_outcomes(env, "staging")
        self.assertTrue(outcomes)
        self.assertTrue(all(sample == ("success", "closed") for sample in outcomes))

    def test_oversized_history_response_reports_cap_not_truncated_parse(self):
        # A body over the cap must fail as "exceeds the cap", never be
        # parsed after silent truncation.
        oversized = json.dumps({"pad": "x" * ((1 << 20) + 100)}).encode()
        base = self.serve_history(lambda _query: oversized, oversized)
        with mock.patch.object(history, "API", base), \
                mock.patch.object(urllib.request, "urlopen", _REAL_URLOPEN):
            with self.assertRaises(history.HistoryError) as raised:
                history._get("/repos/owner/repo/actions/workflows/x/runs", "t")
        self.assertIn("exceeds", str(raised.exception))

    def test_truncated_history_read_reports_history_error(self):
        # A mid-read truncation raises http.client.IncompleteRead (not an
        # OSError): it must become HistoryError so fail-open still applies
        # instead of crashing the decide step.
        import http.client
        opener = mock.Mock()
        opener.open.side_effect = http.client.IncompleteRead(b"", 10)
        with mock.patch.object(history.urllib.request, "build_opener",
                               return_value=opener):
            with self.assertRaisesRegex(history.HistoryError, "IncompleteRead"):
                history._get("/repos/owner/repo/actions/workflows/x/runs", "t")

    def test_non_discord_destination_refuses_before_any_request(self):
        with self.assertRaisesRegex(alert.AlertError, "not a Discord webhook"):
            alert.post("https://example.com/hook", "content")

    def test_http_error_reports_status_only(self):
        import urllib.error
        opener = mock.Mock()
        opener.open.side_effect = urllib.error.HTTPError(
            HOOK, 400, "bad", {}, None)
        with mock.patch.object(alert.urllib.request, "build_opener", return_value=opener):
            with self.assertRaisesRegex(alert.AlertError, "status 400"):
                alert.post(HOOK, "content")

    def test_missing_webhook_secret_fails_without_a_request(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, {"PROBE_OUTCOME": "failure"}, clear=True), \
                redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes", return_value=[("failure", "success")]), \
                mock.patch.object(alert.urllib.request, "build_opener") as opener:
            code = alert.main(["--env", "staging", "--evidence", self.evidence])
        self.assertEqual(code, 1)
        opener.assert_not_called()
        self.assertIn("not set", err.getvalue())

    def test_missing_probe_sample_and_non_transition_do_not_post(self):
        # A skipped probe is zero samples: fail closed (exit 1) with no post,
        # distinct from a healthy pass. A lone failure below the two-failure
        # threshold still decides no transition and stays green.
        for outcome, previous, expected_code in (("skipped", [], 1), ("failure", [], 0)):
            env = {**self.base_env(), "PROBE_OUTCOME": outcome, "FORCE_FAIL": "false"}
            with self.subTest(outcome=outcome), mock.patch.dict(os.environ, env, clear=True), \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()), \
                    mock.patch.object(alert, "previous_probe_outcomes", return_value=previous), \
                    mock.patch.object(alert, "post") as post:
                code = alert.main(["--env", "staging", "--evidence", self.evidence])
            self.assertEqual(code, expected_code)
            post.assert_not_called()
        # The live decide-step path fails the same way on zero samples while
        # still writing the empty event that keeps the post step skipped, and
        # without spending a history read on a non-sample.
        with tempfile.NamedTemporaryFile(mode="w+", delete=False) as output:
            output_path = output.name
        try:
            env = {**self.base_env(), "PROBE_OUTCOME": "skipped",
                   "FORCE_FAIL": "false", "GITHUB_OUTPUT": output_path}
            with mock.patch.dict(os.environ, env, clear=True), \
                    redirect_stderr(io.StringIO()), \
                    mock.patch.object(alert, "previous_probe_outcomes",
                                      side_effect=AssertionError("no sample must not read history")):
                code = alert.main(["--env", "staging", "--evidence", self.evidence,
                                   "--decide-only"])
            self.assertEqual(code, 1)
            self.assertEqual(Path(output_path).read_text(), "event=\n")
        finally:
            Path(output_path).unlink(missing_ok=True)

    def test_history_error_does_not_suppress_failure_alert(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, {**self.base_env(), "FORCE_FAIL": "false"}, clear=True), \
                redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes",
                                  side_effect=history.HistoryError("history unavailable")), \
                mock.patch.object(alert, "post") as post:
            code = alert.main(["--env", "staging", "--evidence", self.evidence])
        self.assertEqual(code, 0)
        post.assert_called_once()
        self.assertIn("sending failure alert without history", err.getvalue())

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
        opener = mock.Mock()
        opener.open.side_effect = fake
        with mock.patch.dict(os.environ, self.base_env(), clear=True), \
                redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes", return_value=[]), \
                mock.patch.object(alert.urllib.request, "build_opener", return_value=opener):
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
        opener = mock.Mock()
        opener.open.return_value = FakeResponse()
        with mock.patch.dict(os.environ, self.base_env(), clear=True), \
                redirect_stdout(io.StringIO()), redirect_stderr(err), \
                mock.patch.object(alert, "previous_probe_outcomes", return_value=[]), \
                mock.patch.object(alert.urllib.request, "build_opener", return_value=opener):
            code = alert.main(["--env", "prod", "--evidence", self.evidence])
        self.assertEqual(code, 1)
        self.assertIn("status 500", err.getvalue())

    def test_deeply_nested_history_body_is_unknown_history_not_a_crash(self):
        # A response far under the size cap can still nest past the JSON
        # recursion limit: it must map to the fixed HistoryError vocabulary
        # (fail-open on a failed probe), never escape as a traceback.
        body = '{"a":' * 10000 + '1' + '}' * 10000
        self.assertLess(len(body.encode("utf-8")), history.MAX_RESPONSE_BYTES)

        class FakeResponse:
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, _n):
                return body.encode("utf-8")

        opener = mock.Mock()
        opener.open.return_value = FakeResponse()
        env = {**self.base_env(), "GITHUB_REPOSITORY": "owner/repo",
               "GITHUB_RUN_ID": "999"}
        with mock.patch.object(history.urllib.request, "build_opener",
                               return_value=opener):
            with self.assertRaises(history.HistoryError) as raised:
                history._get("/repos/owner/repo/actions/runs", "test-token")
            self.assertEqual(
                str(raised.exception),
                "could not read recent synthetic workflow history (RecursionError)")
            err = io.StringIO()
            with redirect_stderr(err):
                event = alert.decide_event({**env, "PROBE_OUTCOME": "failure",
                                            "FORCE_FAIL": "false"}, "staging")
        self.assertEqual(event, "alert")
        self.assertNotIn("Traceback", err.getvalue())

    def test_deeply_nested_evidence_reads_as_unknown_build(self):
        # An unparseably deep receipt follows the existing malformed-evidence
        # channel (unknown build, still posts), never a traceback.
        deep = '{"a":' * 10000 + '1' + '}' * 10000
        path = str(self.dir / "deep-evidence.json")
        Path(path).write_text(deep, encoding="utf-8")
        self.assertIsNone(alert.load_evidence(path))
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err), \
                mock.patch.dict(os.environ, self.base_env(), clear=True), \
                mock.patch.object(alert, "post") as post:
            code = alert.post_event(self.base_env(), "staging", path, "alert")
        self.assertEqual(code, 0)
        post.assert_called_once()
        self.assertIn("unknown", post.call_args.args[1])
        self.assertNotIn("Traceback", out.getvalue() + err.getvalue())


class MalformedHistoryAcceptanceTests(unittest.TestCase):
    """Protected, offline history-admission and decision contract.

    Check: python3 -m unittest discover -s scripts -p test_synthetic_alert.py -v
    Admission failures are HistoryError(code="history_unknown",
    next_action="inspect_workflow_history"); diagnostics never echo remote data.
    Optional --decide-only --json emits one bounded JSON object on stdout:
    schema_version=1, check_id=synthetic-user-flow:<env>, run_id=<current run>,
    event=alert|recovery|null, history_known=bool, code=history_unknown|decision_ready,
    next_action=inspect_workflow_history|none. GITHUB_OUTPUT remains event=<value>.
    No JSON opt-in changes posting, debounce, history bounds or checkpoint policy.
    """

    SENTINEL = "REMOTE_SECRET_SENTINEL_" + "untrusted-" * 512

    def setUp(self):
        tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.output = self.dir / "github-output"
        for obj, name in ((urllib.request, "urlopen"),
                          (urllib.request, "build_opener"), (alert, "post")):
            guard = mock.patch.object(obj, name, side_effect=AssertionError(
                "external transport or posting is forbidden in decide-only fixtures"))
            guard.start()
            self.addCleanup(guard.stop)

    def env(self, current="failure"):
        # No webhook or real credential is needed to decide an event.
        return {"GITHUB_TOKEN": "offline-test-value", "GITHUB_REPOSITORY": "owner/repo",
                "GITHUB_RUN_ID": "999", "RUN_ID": "999", "FORCE_FAIL": "false",
                "PROBE_OUTCOME": current, "GITHUB_OUTPUT": str(self.output)}

    def run_row(self, run_id=100, number=99):
        return {"id": run_id, "run_number": number, "event": "schedule",
                "status": "completed", "conclusion": "success"}

    def run_page(self):
        return {"total_count": 2, "workflow_runs": [self.run_row(999, 100), self.run_row()]}

    def jobs_page(self, environment="staging", probe="success", delivery="skipped",
                  decide="success", job=None):
        return {"total_count": 1, "jobs": [{
            "id": 200, "name": history.JOB_NAMES[environment], "status": "completed",
            "conclusion": job or probe, "steps": [
                {"number": 1, "name": history.PROBE_STEPS[environment],
                 "status": "completed", "conclusion": probe},
                {"number": 2, "name": history.DECIDE_STEPS[environment],
                 "status": "completed", "conclusion": decide},
                {"number": 3, "name": history.ALERT_STEPS[environment],
                 "status": "completed", "conclusion": delivery},
            ]}]}

    def malformed_cases(self, environment="staging"):
        """Independent one-field mutations of complete Actions-shaped controls."""
        import copy

        cases = []
        for label, value in (("null", None), ("array", []), ("text", self.SENTINEL),
                             ("boolean", True), ("integer", 7)):
            cases.extend((("runs-root-" + label, value, self.jobs_page(environment)),
                          ("jobs-root-" + label, self.run_page(), value)))
        for label, value in (("missing", {}), ("null", None),
                             ("object", {"poison": self.SENTINEL}),
                             ("text", self.SENTINEL), ("member", [None])):
            runs, jobs = self.run_page(), self.jobs_page(environment)
            if label == "missing":
                del runs["workflow_runs"]
                del jobs["jobs"]
            else:
                runs["workflow_runs"], jobs["jobs"] = value, value
            cases.extend((("runs-collection-" + label, runs, self.jobs_page(environment)),
                          ("jobs-collection-" + label, self.run_page(), jobs)))
        bad_numbers = (None, True, False, 0, -1, 1.5, 100.0, float("nan"),
                       float("inf"), "100", self.SENTINEL, [], {})
        for field in ("id", "run_number"):
            for index, value in enumerate(bad_numbers):
                runs = self.run_page()
                runs["workflow_runs"][1][field] = value
                cases.append((f"run-{field}-{index}", runs, self.jobs_page(environment)))
            runs = self.run_page()
            del runs["workflow_runs"][1][field]
            cases.append(("run-missing-" + field, runs, self.jobs_page(environment)))
        for root in ("runs", "jobs"):
            for index, value in enumerate((None, True, -1, 1.5, "2", self.SENTINEL, [], {})):
                runs, jobs = self.run_page(), self.jobs_page(environment)
                (runs if root == "runs" else jobs)["total_count"] = value
                cases.append((f"{root}-count-{index}", runs, jobs))
            runs, jobs = self.run_page(), self.jobs_page(environment)
            (runs if root == "runs" else jobs)["total_count"] = 0
            cases.append((root + "-count-less-than-returned", runs, jobs))
        for root, member in (("runs", "workflow_runs"), ("jobs", "jobs")):
            for index, value in enumerate((None, self.SENTINEL, True, [])):
                runs, jobs = self.run_page(), self.jobs_page(environment)
                body = runs if root == "runs" else jobs
                body[member].append(value)
                body["total_count"] += 1
                cases.append((f"{root}-mixed-member-{index}", runs, jobs))
        for index, value in enumerate((None, True, 3, [], {})):
            runs = self.run_page()
            runs["workflow_runs"][1]["event"] = value
            cases.append((f"run-event-{index}", runs, self.jobs_page(environment)))
        jobs = self.jobs_page(environment)
        jobs["total_count"] = 2
        cases.append(("jobs-incomplete-count", self.run_page(), jobs))
        runs = self.run_page()
        runs["workflow_runs"].append(copy.deepcopy(runs["workflow_runs"][1]))
        runs["total_count"] = 3
        cases.append(("duplicate-run-id", runs, self.jobs_page(environment)))
        runs = self.run_page()
        runs["workflow_runs"].append(self.run_row(101, 99))
        runs["total_count"] = 3
        cases.append(("ambiguous-run-order", runs, self.jobs_page(environment)))
        for label, value in (("null", None), ("text", self.SENTINEL),
                             ("object", {}), ("boolean", False)):
            jobs = self.jobs_page(environment)
            jobs["jobs"][0]["steps"] = value
            cases.append(("steps-collection-" + label, self.run_page(), jobs))
        for label, value in (("null", None), ("text", self.SENTINEL), ("array", [])):
            jobs = self.jobs_page(environment)
            jobs["jobs"][0]["steps"].append(value)
            cases.append(("step-member-" + label, self.run_page(), jobs))
        for location in ("job", "probe", "decide", "alert"):
            for index, value in enumerate((self.SENTINEL, True, 3, [], {})):
                jobs = self.jobs_page(environment)
                target = jobs["jobs"][0]
                if location != "job":
                    target = target["steps"][("probe", "decide", "alert").index(location)]
                target["conclusion"] = value
                cases.append((f"{location}-unsupported-conclusion-{index}", self.run_page(), jobs))
            for index, value in enumerate((None, [], {}, 3)):
                jobs = self.jobs_page(environment)
                target = jobs["jobs"][0]
                if location != "job":
                    target = target["steps"][("probe", "decide", "alert").index(location)]
                target["name"] = value
                cases.append((f"{location}-malformed-name-{index}", self.run_page(), jobs))
        # Identical duplicates are still ambiguous; a conflicting second match
        # must not be ignored just because a plausible first match was found.
        for location in ("job", "probe", "decide", "alert"):
            for conflicting in (False, True):
                for reverse in (False, True):
                    jobs = self.jobs_page(environment)
                    items = jobs["jobs"] if location == "job" else jobs["jobs"][0]["steps"]
                    index = 0 if location == "job" else ("probe", "decide", "alert").index(location)
                    duplicate = copy.deepcopy(items[index])
                    if conflicting:
                        duplicate["conclusion"] = "failure"
                    items.append(duplicate)
                    if location == "job":
                        jobs["total_count"] = 2
                    if reverse:
                        items.reverse()
                    cases.append((f"duplicate-{location}-{conflicting}-{reverse}",
                                  self.run_page(), jobs))
        return cases

    def reader(self, runs, jobs):
        import copy
        import re
        from urllib.parse import urlparse, parse_qs

        calls = []

        def get(path, _token):
            calls.append(path)
            if "/actions/workflows/" in path:
                # A malformed full first page must not be silently skipped.
                page = int(parse_qs(urlparse(path).query).get("page", ["1"])[0])
                return copy.deepcopy(runs if page == 1 else {"total_count": 2, "workflow_runs": []})
            if not re.search(r"/actions/runs/(100|99)/jobs\?", path):
                raise AssertionError("history constructed a jobs URL from an unadmitted ID")
            return copy.deepcopy(jobs)

        return get, calls

    def assert_safe_history_error(self, operation):
        try:
            operation()
        except history.HistoryError as error:
            self.assertEqual(getattr(error, "code", None), "history_unknown")
            self.assertEqual(getattr(error, "next_action", None), "inspect_workflow_history")
            self.assertLessEqual(len(str(error)), 240)
            self.assertIn("history", str(error).lower())
            self.assertNotIn("REMOTE_SECRET_SENTINEL", str(error))
            self.assertNotIn("untrusted-", str(error))
            self.assertNotIn("Traceback", str(error))
        except Exception as error:
            self.fail(f"{type(error).__name__} escaped instead of a safe HistoryError")
        else:
            self.fail("malformed or ambiguous history was accepted as known history")

    def invoke(self, get, current, environment="staging", json_mode=False):
        self.output.unlink(missing_ok=True)
        out, err = io.StringIO(), io.StringIO()
        args = ["--env", environment, "--evidence", str(self.dir / "absent-evidence.json"),
                "--decide-only"] + (["--json"] if json_mode else [])
        with mock.patch.dict(os.environ, self.env(current), clear=True), \
                mock.patch.object(history, "_get", side_effect=get), \
                redirect_stdout(out), redirect_stderr(err):
            try:
                code = alert.main(args)
            except (Exception, SystemExit) as error:
                self.fail(f"decide-only escaped {type(error).__name__} instead of returning a decision")
        stdout, stderr = out.getvalue(), err.getvalue()
        for text in (stdout, stderr):
            self.assertNotIn("REMOTE_SECRET_SENTINEL", text)
            self.assertNotIn("untrusted-", text)
            self.assertNotIn("offline-test-value", text)
            self.assertNotIn("Traceback", text)
            self.assertLessEqual(len(text.encode("utf-8")), 2048)
        self.assertTrue(self.output.exists(), "decide-only lost its GITHUB_OUTPUT record")
        return code, stdout, stderr, self.output.read_text()

    def test_malformed_runs_are_refused_before_jobs_url_or_checkpoint(self):
        for label, runs, jobs in self.malformed_cases():
            if not label.startswith(("run", "duplicate-run", "ambiguous-run")):
                continue
            with self.subTest(case=label):
                get, calls = self.reader(runs, jobs)
                with mock.patch.object(history, "_get", side_effect=get):
                    self.assert_safe_history_error(lambda: history.previous_probe_outcomes(
                        self.env(), "staging"))
                self.assertFalse(any("/jobs?" in path for path in calls),
                                 "run-page admission must precede jobs fetches")

    def test_malformed_jobs_and_ambiguous_steps_are_safe_history_errors(self):
        for label, runs, jobs in self.malformed_cases():
            if label.startswith(("run", "duplicate-run", "ambiguous-run")):
                continue
            with self.subTest(case=label):
                get, _ = self.reader(runs, jobs)
                with mock.patch.object(history, "_get", side_effect=get):
                    self.assert_safe_history_error(lambda: history._run_outcome(
                        "owner/repo", self.run_row(), "staging", "offline-test-value"))

    def test_every_malformed_case_preserves_failed_and_healthy_decide_policy(self):
        # Running the actual entry point catches raw exceptions that bypass the
        # HistoryError policy, not merely parsers that reject a fixture in isolation.
        for environment in ("staging", "prod"):
            for label, runs, jobs in self.malformed_cases(environment):
                for current, want_code, want_event in (("failure", 0, "event=alert\n"),
                                                       ("success", 1, "event=\n")):
                    with self.subTest(env=environment, case=label, current=current):
                        get, _ = self.reader(runs, jobs)
                        code, stdout, stderr, output = self.invoke(get, current, environment)
                        self.assertEqual(code, want_code)
                        self.assertEqual(output, want_event)
                        self.assertEqual(stdout, "", "text-mode decision must stay stdout-compatible")
                        self.assertIn("synthetic_alert:", stderr)

    def test_invalid_filtered_row_cannot_hide_behind_a_valid_checkpoint(self):
        for event, run_id in (("workflow_dispatch", 101), ("schedule", 1000)):
            runs = self.run_page()
            runs["workflow_runs"].append({"id": run_id, "event": event,
                                           "run_number": self.SENTINEL})
            runs["total_count"] = 3
            get, calls = self.reader(runs, self.jobs_page())
            with self.subTest(event=event), mock.patch.object(history, "_get", side_effect=get):
                self.assert_safe_history_error(lambda: history.previous_probe_outcomes(
                    self.env(), "staging"))
                self.assertEqual(len(calls), 1)

    def test_duplicate_ids_or_order_on_later_pages_are_not_replayed_twice(self):
        from urllib.parse import urlparse, parse_qs

        for duplicate in (self.run_row(100, 98), self.run_row(99, 99)):
            calls = []

            def get(path, _token):
                calls.append(path)
                if "/actions/workflows/" in path:
                    page = int(parse_qs(urlparse(path).query).get("page", ["1"])[0])
                    rows = {1: [self.run_row(999, 100), self.run_row()],
                            2: [duplicate, self.run_row(98, 97)]}.get(page, [])
                    return {"total_count": 4, "workflow_runs": rows}
                return self.jobs_page(probe="failure", delivery="skipped", decide="failure")

            with self.subTest(id=duplicate["id"], number=duplicate["run_number"]), \
                    mock.patch.object(history, "RUNS_PAGE_SIZE", 2), \
                    mock.patch.object(history, "_get", side_effect=get):
                self.assert_safe_history_error(lambda: history.previous_probe_outcomes(
                    self.env(), "staging"))
                self.assertLessEqual(sum("/jobs?" in path for path in calls), 1,
                                     "second-page admission must precede replay")

    def test_json_option_is_discoverable_without_touching_history(self):
        out = io.StringIO()
        with redirect_stdout(out), self.assertRaises(SystemExit) as exit_info:
            alert.main(["--help"])
        self.assertEqual(exit_info.exception.code, 0)
        self.assertIn("--json", out.getvalue())

    def test_json_unknown_history_is_bounded_identified_and_keeps_event_output(self):
        for environment in ("staging", "prod"):
            for current, want_code, want_event in (("failure", 0, "alert"), ("success", 1, None)):
                get, _ = self.reader(self.run_page(), {"jobs": [None]})
                with self.subTest(env=environment, current=current):
                    code, stdout, _, output = self.invoke(get, current, environment, json_mode=True)
                    self.assertEqual(code, want_code)
                    self.assertEqual(output, f"event={want_event or ''}\n")
                    receipt = json.loads(stdout)
                    self.assertEqual(receipt["schema_version"], 1)
                    self.assertEqual(receipt["check_id"], f"synthetic-user-flow:{environment}")
                    self.assertEqual(receipt["run_id"], "999")
                    self.assertEqual(receipt["event"], want_event)
                    self.assertIs(receipt["history_known"], False)
                    self.assertEqual(receipt["code"], "history_unknown")
                    self.assertEqual(receipt["next_action"], "inspect_workflow_history")

    def test_json_valid_history_uses_the_same_transition_as_text_mode(self):
        for current, probe, delivery, want_event in (
                ("failure", "success", "skipped", None),
                ("failure", "failure", "skipped", "alert"),
                ("success", "failure", "success", "recovery")):
            # Explicit failed decide prevents this one-sample failure fixture
            # from pretending a suppressed alert was already delivered.
            jobs = self.jobs_page(probe=probe, delivery=delivery, decide="failure")
            with self.subTest(current=current, probe=probe, delivery=delivery):
                runs = self.run_page()
                runs["workflow_runs"].append(self.run_row(99, 98))
                runs["total_count"] = 3
                get, _ = self.reader(runs, jobs)
                text_code, _, _, text_event = self.invoke(get, current)
                self.assertEqual(text_code, 0)
                self.assertEqual(text_event, f"event={want_event or ''}\n")
                get, _ = self.reader(runs, jobs)
                code, stdout, _, output = self.invoke(get, current, json_mode=True)
                receipt = json.loads(stdout)
                self.assertEqual(code, text_code)
                self.assertEqual(output, text_event)
                self.assertEqual(receipt["event"], want_event)
                self.assertIs(receipt["history_known"], True)
                self.assertEqual(receipt["code"], "decision_ready")
                self.assertEqual(receipt["next_action"], "none")

    def test_empty_and_unrelated_valid_history_keep_debounce_and_filtering(self):
        for environment in ("staging", "prod"):
            for current in ("success", "failure"):
                with self.subTest(env=environment, current=current, history="empty"):
                    get, calls = self.reader({"total_count": 0, "workflow_runs": []},
                                             self.jobs_page(environment))
                    code, _, _, output = self.invoke(get, current, environment)
                    self.assertEqual((code, output), (0, "event=\n"))
                    self.assertEqual(len(calls), 1)
            runs, jobs = self.run_page(), self.jobs_page(environment)
            ignored_run = self.run_row(102, 101)
            ignored_run["event"] = "workflow_dispatch"
            runs["workflow_runs"].insert(0, ignored_run)
            runs["total_count"] = 3
            jobs["jobs"].insert(0, {"id": 201, "name": "other validation job",
                                      "status": "completed", "conclusion": "success", "steps": []})
            jobs["total_count"] = 2
            jobs["jobs"][1]["steps"].insert(0, {"number": 1, "name": "Set up job",
                                                "status": "completed", "conclusion": "success"})
            for number, step in enumerate(jobs["jobs"][1]["steps"], 1):
                step["number"] = number
            with self.subTest(env=environment, history="unrelated entries"):
                get, _ = self.reader(runs, jobs)
                code, _, _, output = self.invoke(get, "failure", environment)
                self.assertEqual((code, output), (0, "event=\n"))

    def test_zero_sample_still_fails_without_history_or_posting(self):
        def forbidden_get(*_args):
            self.fail("a skipped current probe must not read history")

        code, _, _, output = self.invoke(forbidden_get, "skipped")
        self.assertEqual(code, 1)
        self.assertEqual(output, "event=\n")

    def test_cancelled_timed_out_or_skipped_delivery_is_never_delivery_or_closure(self):
        for delivery in ("cancelled", "timed_out", "skipped"):
            for job in ("cancelled", "timed_out", "success"):
                jobs = self.jobs_page(probe="success", delivery=delivery, job=job)
                # A successful decide in a normal job plus skipped delivery is
                # the existing closed checkpoint; it is not an undelivered recovery.
                if job == "success" and delivery == "skipped":
                    continue
                with self.subTest(delivery=delivery, job=job):
                    get, _ = self.reader(self.run_page(), jobs)
                    with mock.patch.object(history, "_get", side_effect=get):
                        try:
                            sample = history._run_outcome("owner/repo", self.run_row(),
                                                          "staging", "offline-test-value")
                        except history.HistoryError:
                            continue  # Refusal is safe; inventing a checkpoint is not.
                    self.assertIsNotNone(sample)
                    self.assertNotIn(sample[1], ("success", "closed", "decided_none"))

    def timeline(self, samples, job_conclusions=None):
        fake = SyntheticAlertTests.fake_history(self, samples, job_conclusions=job_conclusions)

        def get(path, token):
            body = fake(path, token)
            if "workflow_runs" in body:
                for run in body["workflow_runs"]:
                    if run["id"] == 99999:
                        run["id"] = 999
            return body

        return get, fake

    def test_interrupted_delivery_keeps_alert_or_recovery_due_end_to_end(self):
        for delivery in ("cancelled", "timed_out", "skipped"):
            for interrupted_job in ("cancelled", "timed_out"):
                with self.subTest(delivery=delivery, job=interrupted_job):
                    samples = [("failure", delivery)] * 6
                    get, _ = self.timeline(samples, {i: interrupted_job for i in range(6)})
                    code, _, _, output = self.invoke(get, "failure")
                    self.assertEqual((code, output), (0, "event=alert\n"))
                    samples = [("failure", "skipped"), ("failure", "success"),
                               ("success", delivery)]
                    get, _ = self.timeline(samples, {2: interrupted_job})
                    code, _, _, output = self.invoke(get, "success")
                    self.assertIn((code, output), ((0, "event=recovery\n"), (1, "event=\n")),
                                  "an interrupted recovery must not close a delivered incident")

    def test_existing_quiet_and_long_outage_controls_reach_actual_decide_entry(self):
        controls = (
            ([("success", "skipped")] * 250, "failure", None),
            ([("success", "skipped")] * 250, "success", None),
            ([("success", "skipped"), ("failure", "skipped")], "failure", "alert"),
            ([("failure", "skipped"), ("failure", "success")] +
             [("failure", "skipped")] * 128, "failure", None),
            ([("failure", "skipped"), ("failure", "success")] +
             [("failure", "skipped")] * 128, "success", "recovery"),
        )
        for samples, current, want_event in controls:
            with self.subTest(length=len(samples), current=current):
                get, fake = self.timeline(samples)
                code, _, _, output = self.invoke(get, current)
                self.assertEqual(code, 0)
                self.assertEqual(output, f"event={want_event or ''}\n")
                self.assertLess(fake.jobs_calls, 10, "checkpoint regressed to unbounded replay")


if __name__ == "__main__":
    unittest.main()

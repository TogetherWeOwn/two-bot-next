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
        for outcome, previous in (("skipped", []), ("failure", [])):
            env = {**self.base_env(), "PROBE_OUTCOME": outcome, "FORCE_FAIL": "false"}
            with self.subTest(outcome=outcome), mock.patch.dict(os.environ, env, clear=True), \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()), \
                    mock.patch.object(alert, "previous_probe_outcomes", return_value=previous), \
                    mock.patch.object(alert, "post") as post:
                code = alert.main(["--env", "staging", "--evidence", self.evidence])
            self.assertEqual(code, 0)
            post.assert_not_called()

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


if __name__ == "__main__":
    unittest.main()

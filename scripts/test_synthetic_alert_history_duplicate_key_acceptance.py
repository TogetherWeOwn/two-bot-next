"""Protected offline duplicate-key refusal for the synthetic history reader.

Failing-first acceptance for `synthetic_alert_history._get`: bare
`json.loads` parses duplicate object keys last-wins, so a corrupted history
body can decide a wrong alert verdict instead of reporting unknown history.
Offline only: the GitHub transport is a fake opener, no network, no
credentials.
"""

import contextlib
import importlib
import io
import json
import sys
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
history = importlib.import_module("synthetic_alert_history")
alert = importlib.import_module("synthetic_alert")

EXPECTED = "could not read recent synthetic workflow history (DuplicateKeyError)"
SENTINEL = "hostile-dupkey-history-sentinel"
ENV = {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
       "GITHUB_RUN_ID": "99999"}


def valid_runs_body():
    runs = [{"id": 100 + index, "run_number": index, "event": "schedule"}
            for index in (1, 2, 3)]
    return json.dumps({"total_count": len(runs), "workflow_runs": runs})


def valid_jobs_body():
    return json.dumps({"jobs": [{
        "name": history.JOB_NAMES["staging"], "conclusion": "failure",
        "steps": [
            {"name": history.PROBE_STEPS["staging"], "conclusion": "failure"},
            {"name": history.DECIDE_STEPS["staging"], "conclusion": "success"},
            {"name": history.ALERT_STEPS["staging"], "conclusion": "success"},
        ]}]})


class _FakeResponse:
    def __init__(self, body):
        self._body = body if isinstance(body, bytes) else body.encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def read(self, size=-1):
        if size is None or size < 0:
            return self._body
        return self._body[:size]


class _Router:
    """Fake opener serving one runs page and one jobs body by URL."""

    def __init__(self, runs, jobs):
        self.runs = runs
        self.jobs = jobs

    def open(self, request, timeout=None):
        body = self.jobs if "/jobs" in request.get_full_url() else self.runs
        return _FakeResponse(body)


class HistoryDuplicateKeyAcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(
            history.urllib.request, "build_opener",
            side_effect=AssertionError("offline acceptance must not touch the network"))
        guard.start()
        self.addCleanup(guard.stop)

    @contextlib.contextmanager
    def served(self, runs, jobs):
        with mock.patch.object(history.urllib.request, "build_opener",
                               return_value=_Router(runs, jobs)):
            yield

    def expect_history_error(self, call):
        try:
            result = call()
        except history.HistoryError as error:
            message = str(error)
            self.assertEqual(message, EXPECTED)
            self.assertLess(len(message), 160)
            self.assertNotIn(SENTINEL, message)
            return
        except Exception as error:
            self.fail(f"expected fixed-vocabulary HistoryError, got {type(error).__name__}")
        self.fail(f"duplicate keys must raise HistoryError, not parse last-wins: {result!r}")

    def test_get_refuses_conflicting_duplicate_keys_both_orders(self):
        for first, second in (("schedule", "workflow_dispatch"),
                              ("workflow_dispatch", "schedule")):
            with self.subTest(first=first, second=second):
                body = ('{"total_count": 1, "workflow_runs": [{"id": 7,'
                        f'"run_number": 1, "event": {json.dumps(first)}, "event": {json.dumps(second)}}}]}}')
                with self.served(body, valid_jobs_body()):
                    self.expect_history_error(lambda: history._get("/runs", "t"))

    def test_get_refuses_identical_duplicate_values(self):
        body = ('{"total_count": 1, "workflow_runs": [{"id": 7,'
                '"run_number": 1, "event": "schedule", "event": "schedule"}]}')
        with self.served(body, valid_jobs_body()):
            self.expect_history_error(lambda: history._get("/runs", "t"))

    def test_get_refuses_nested_duplicate_keys(self):
        body = ('{"total_count": 1, "workflow_runs": [{"id": 7,'
                '"run_number": 1, "event": "schedule",'
                '"head_commit": {"message": "one", "message": "two"}}]}')
        with self.served(body, valid_jobs_body()):
            self.expect_history_error(lambda: history._get("/runs", "t"))

    def test_get_refuses_duplicate_probe_conclusions_both_orders(self):
        name = history.PROBE_STEPS["staging"]
        for first, second in (("failure", "success"), ("success", "failure")):
            with self.subTest(first=first, second=second):
                body = ('{"jobs": [{"name": ' + json.dumps(history.JOB_NAMES["staging"])
                        + ', "conclusion": "failure", "steps": [{"name": '
                        + json.dumps(name)
                        + f', "conclusion": {json.dumps(first)}, "conclusion": {json.dumps(second)}}}]}}]}}')
                with self.served(valid_runs_body(), body):
                    self.expect_history_error(lambda: history._get("/jobs", "t"))

    def test_get_refusal_carries_no_hostile_bytes(self):
        hostile = ('{"total_count": 1, "workflow_runs": [{'
                   f'"id": 7, "run_number": 1, "{SENTINEL}": "{SENTINEL}", '
                   f'"{SENTINEL}": "{SENTINEL}", "event": "schedule"}}]}}')
        with self.served(hostile, valid_jobs_body()):
            self.expect_history_error(lambda: history._get("/runs", "t"))
        hostile_jobs = ('{"jobs": [{"name": ' + json.dumps(history.JOB_NAMES["staging"])
                        + ', "conclusion": "failure", "steps": [{"name": '
                        + json.dumps(history.PROBE_STEPS["staging"])
                        + f', "{SENTINEL}": 1, "{SENTINEL}": 2}}]}}]}}')
        with self.served(valid_runs_body(), hostile_jobs):
            self.expect_history_error(lambda: history._get("/jobs", "t"))

    def test_get_unique_bodies_decode_byte_identical(self):
        runs, jobs = valid_runs_body(), valid_jobs_body()
        with self.served(runs, jobs):
            self.assertEqual(history._get("/runs", "t"), json.loads(runs))
            self.assertEqual(history._get("/jobs", "t"), json.loads(jobs))
            self.assertEqual(history._get("/runs", "t"), history._get("/runs", "t"))

    def duplicate_runs_bodies(self):
        conflict = ('{"total_count": 3, "workflow_runs": ['
                    '{"id": 101, "run_number": 1, "event": "schedule",'
                    ' "event": "workflow_dispatch"},'
                    '{"id": 102, "run_number": 2, "event": "schedule"},'
                    '{"id": 103, "run_number": 3, "event": "schedule"}]}')
        identical = ('{"total_count": 3, "workflow_runs": ['
                     '{"id": 101, "run_number": 1, "event": "schedule"},'
                     '{"id": 102, "run_number": 2, "event": "schedule",'
                     ' "event": "schedule"},'
                     '{"id": 103, "run_number": 3, "event": "schedule"}]}')
        nested = ('{"total_count": 3, "workflow_runs": ['
                  '{"id": 101, "run_number": 1, "event": "schedule"},'
                  '{"id": 102, "run_number": 2, "event": "schedule"},'
                  '{"id": 103, "run_number": 3, "event": "schedule",'
                  ' "head_commit": {"message": "one", "message": "two"}}]}')
        hostile = ('{"total_count": 3, "workflow_runs": ['
                   f'{{"id": 101, "run_number": 1, "{SENTINEL}": 1,'
                   f' "{SENTINEL}": 2, "event": "schedule"}},'
                   '{"id": 102, "run_number": 2, "event": "schedule"},'
                   '{"id": 103, "run_number": 3, "event": "schedule"}]}')
        reversed_order = ('{"total_count": 3, "workflow_runs": ['
                            '{"id": 101, "run_number": 1, "event": "workflow_dispatch",'
                            ' "event": "schedule"},'
                            '{"id": 102, "run_number": 2, "event": "schedule"},'
                            '{"id": 103, "run_number": 3, "event": "schedule"}]}')
        return {"conflicting": conflict, "reversed": reversed_order,
                "identical": identical, "nested": nested, "hostile": hostile}

    def duplicate_jobs_bodies(self):
        job = history.JOB_NAMES["staging"]
        probe = history.PROBE_STEPS["staging"]
        decide = history.DECIDE_STEPS["staging"]
        post = history.ALERT_STEPS["staging"]

        def jobs(step_fragment):
            return ('{"jobs": [{"name": ' + json.dumps(job)
                    + ', "conclusion": "failure", "steps": [{"name": '
                    + json.dumps(probe) + ', ' + step_fragment + '}, {"name": '
                    + json.dumps(decide)
                    + ', "conclusion": "success"}, {"name": '
                    + json.dumps(post) + ', "conclusion": "success"}]}]}')

        return {
            "conflicting": jobs('"conclusion": "failure", "conclusion": "success"'),
            "reversed": jobs('"conclusion": "success", "conclusion": "failure"'),
            "identical": jobs('"conclusion": "failure", "conclusion": "failure"'),
            "nested": jobs(f'"{SENTINEL}": 1, "{SENTINEL}": 2, "conclusion": "failure"'),
        }

    def test_previous_outcomes_refuses_duplicate_runs_body(self):
        for family, body in self.duplicate_runs_bodies().items():
            with self.subTest(family=family):
                with self.served(body, valid_jobs_body()):
                    self.expect_history_error(
                        lambda: history.previous_probe_outcomes(ENV, "staging"))

    def test_previous_outcomes_refuses_duplicate_jobs_body(self):
        for family, body in self.duplicate_jobs_bodies().items():
            with self.subTest(family=family):
                with self.served(valid_runs_body(), body):
                    self.expect_history_error(
                        lambda: history.previous_probe_outcomes(ENV, "staging"))

    def test_previous_outcomes_valid_history_unchanged(self):
        with self.served(valid_runs_body(), valid_jobs_body()):
            first = history.previous_probe_outcomes(ENV, "staging")
            second = history.previous_probe_outcomes(ENV, "staging")
        self.assertEqual(first, second)
        self.assertTrue(first)
        self.assertTrue(all(sample == ("failure", "success") for sample in first))

    def test_refused_history_yields_no_body_derived_recovery(self):
        # Last-wins parses the fixtures below to a delivered incident, so a
        # healthy probe decides "recovery" from the refused body. Unknown
        # history must decide nothing instead (green stays green).
        cases = [(f"runs:{family}", body, valid_jobs_body())
                 for family, body in self.duplicate_runs_bodies().items()]
        # The failure-then-success jobs order last-wins to a healthy sample,
        # which decides None with or without the fix; every other order
        # last-wins to a delivered incident and wrongly decides "recovery".
        cases += [(f"jobs:{key}", valid_runs_body(), body)
                  for key, body in self.duplicate_jobs_bodies().items()
                  if key != "conflicting"]
        for label, runs, jobs in cases:
            with self.subTest(case=label):
                with self.served(runs, jobs):
                    try:
                        outcomes = history.previous_probe_outcomes(ENV, "staging")
                    except history.HistoryError as error:
                        self.assertEqual(str(error), EXPECTED)
                        continue
                    event = history.transition("success", outcomes)
                    self.assertIsNone(
                        event, f"refused {label} body decided {event!r} from {outcomes!r}")

    def test_unknown_history_policy_unchanged(self):
        # The pre-existing fail-open mapping for unknown history is out of
        # scope: a refused body must behave exactly like any other
        # HistoryError (failure alerts without history, success stays green).
        with mock.patch.object(history, "_get",
                               side_effect=history.HistoryError(EXPECTED)), \
                redirect_stderr(io.StringIO()):
            self.assertIsNone(alert.decide_event({**ENV, "PROBE_OUTCOME": "success",
                                                  "FORCE_FAIL": "false"}, "staging"))
            self.assertEqual(alert.decide_event({**ENV, "PROBE_OUTCOME": "failure",
                                                 "FORCE_FAIL": "false"}, "staging"), "alert")


if __name__ == "__main__":
    unittest.main()

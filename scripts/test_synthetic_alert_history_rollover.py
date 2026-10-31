"""Acceptance fixtures: incident delivery must survive history rollover.

A long outage outlives any fixed history window. These offline fixtures
drive the public seams (`synthetic_alert.decide_event`,
`synthetic_alert_history.previous_probe_outcomes` / `transition`) against a
mock paginated GitHub API shaped like the real one: the runs list is
newest-first, pages are selected with `page`/`per_page`, and `per_page` is
capped at 100 exactly as GitHub caps it. Stdlib only, no network access.

Chronological sample shorthand below: each prior scheduled run is a
``(probe_conclusion, alert_post_conclusion)`` pair, oldest first. The
opening alert of an incident is decided on the second consecutive failure,
so a delivered opening alert reads ``("failure", "success")`` at the second
failure and ``("failure", "skipped")`` (post step never ran) after that.
"""

import importlib
import re
import sys
import unittest
import urllib.parse
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
alert = importlib.import_module("synthetic_alert")
history = importlib.import_module("synthetic_alert_history")

ENV_NAME = "staging"
BASE_ID = 10_000


def delivered_outage(failures):
    """Chronological samples for one outage whose opening alert delivered."""
    samples = [("failure", "skipped"), ("failure", "success")]
    samples += [("failure", "skipped")] * (failures - 2)
    return samples


class FakeGitHub:
    """Paginated stand-in for the two GitHub reads history reconstruction uses."""

    def __init__(self, samples, extra_runs=(), fail_page=None, fail_first=False,
                 legacy_runs=0):
        # samples: chronological (probe, alert_step) pairs, oldest first.
        # extra_runs: chronological dicts merged in by position via run_number
        # offsets is overkill; extras are appended as the newest runs instead
        # (dispatch/cancelled noise is position-independent for these tests).
        self.samples = {}
        self.runs = []
        for index, (probe, delivery) in enumerate(samples):
            run_id = BASE_ID + index
            self.samples[run_id] = (probe, delivery)
            self.runs.append({"id": run_id, "run_number": index + 1,
                              "event": "schedule"})
        for offset, extra in enumerate(extra_runs):
            run_id = BASE_ID + len(samples) + offset
            self.samples[run_id] = extra.get("sample", ("skipped", "skipped"))
            self.runs.append({"id": run_id,
                              "run_number": len(samples) + offset + 1,
                              "event": extra.get("event", "schedule")})
        self.current_id = BASE_ID + len(self.runs) + 1
        self.runs.append({"id": self.current_id,
                          "run_number": len(self.runs) + 1, "event": "schedule"})
        self.fail_page = fail_page
        self.fail_first = fail_first
        # The oldest legacy_runs samples carry the pre-issue Discord step name.
        self.legacy_ids = set(range(BASE_ID, BASE_ID + legacy_runs))
        self.runs_calls = 0
        self.jobs_calls = 0

    def __call__(self, path, _token):
        parsed = urllib.parse.urlparse(path)
        query = urllib.parse.parse_qs(parsed.query)
        if "/actions/workflows/" in parsed.path:
            self.runs_calls += 1
            if self.fail_first:
                raise history.HistoryError("history unavailable")
            page = int(query.get("page", ["1"])[0])
            if self.fail_page is not None and page >= self.fail_page:
                raise history.HistoryError("history unavailable")
            # GitHub defaults per_page to 30 and caps it at 100.
            per_page = min(int(query.get("per_page", ["30"])[0]), 100)
            newest_first = sorted(self.runs, key=lambda run: run["run_number"],
                                  reverse=True)
            start = (page - 1) * per_page
            return {"total_count": len(newest_first),
                    "workflow_runs": newest_first[start:start + per_page]}
        match = re.search(r"/actions/runs/(\d+)/jobs", parsed.path)
        if match:
            self.jobs_calls += 1
            probe, delivery = self.samples.get(int(match.group(1)),
                                               ("skipped", "skipped"))
            return {"jobs": [{
                "name": history.JOB_NAMES[ENV_NAME],
                "steps": [
                    {"name": history.PROBE_STEPS[ENV_NAME], "conclusion": probe},
                    {"name": history.DECIDE_STEPS[ENV_NAME],
                     "conclusion": "success"},
                    {"name": (history.LEGACY_ALERT_STEP
                              if int(match.group(1)) in self.legacy_ids
                              else history.ALERT_STEPS[ENV_NAME]),
                     "conclusion": delivery},
                ],
            }]}
        raise AssertionError(f"unexpected history read: {path}")

    def env(self, current):
        return {"GITHUB_TOKEN": "test-token", "GITHUB_REPOSITORY": "owner/repo",
                "GITHUB_RUN_ID": str(self.current_id),
                "PROBE_OUTCOME": current, "FORCE_FAIL": "false"}


class RolloverAcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch("urllib.request.urlopen",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def decide(self, fake, current):
        with mock.patch.object(history, "_get", side_effect=fake):
            return alert.decide_event(fake.env(current), ENV_NAME)

    # -- the reproduced defect: one delivered outage, window rolls over --

    def test_fourteen_failures_no_duplicate_alert_and_recovery_on_green(self):
        fake = FakeGitHub([("success", "skipped")] + delivered_outage(14))
        self.assertIsNone(self.decide(fake, "failure"),
                          "15th failure of an already-alerted outage must not re-alert")
        self.assertEqual(self.decide(fake, "success"), "recovery",
                         "first healthy sample owes the recovery, not an alert")

    def test_outage_alerted_only_to_discord_re_alerts_into_an_issue(self):
        # Delivered by the legacy Discord step, which reached no agent.
        samples = [("success", "skipped")] + delivered_outage(5)
        fake = FakeGitHub(samples, legacy_runs=len(samples))
        self.assertEqual(self.decide(fake, "failure"), "alert")
        self.assertEqual(self.decide(fake, "success"), "alert",
                         "an undelivered incident owes its late alert first")

    def test_issue_delivered_after_a_discord_era_outage_does_not_repeat(self):
        legacy = [("success", "skipped")] + delivered_outage(5)
        fake = FakeGitHub(legacy + [("failure", "success"), ("failure", "skipped")],
                          legacy_runs=len(legacy))
        self.assertIsNone(self.decide(fake, "failure"))
        self.assertEqual(self.decide(fake, "success"), "recovery")

    def test_discord_era_first_failure_still_needs_a_second(self):
        fake = FakeGitHub([("success", "skipped"), ("failure", "skipped")],
                          legacy_runs=2)
        self.assertEqual(self.decide(fake, "failure"), "alert")
        fake = FakeGitHub([("success", "skipped")], legacy_runs=1)
        self.assertIsNone(self.decide(fake, "failure"))

    def test_thirty_failures_no_duplicate_alert_and_recovery_on_green(self):
        fake = FakeGitHub([("success", "skipped")] + delivered_outage(30))
        self.assertIsNone(self.decide(fake, "failure"))
        self.assertEqual(self.decide(fake, "success"), "recovery")

    def test_outage_longer_than_one_github_page_still_reconstructs(self):
        # 130 failures: more than GitHub's per_page cap of 100, so this
        # cannot pass by merely raising a fixed per_page; reconstruction
        # must actually walk pages (bounded, see the bounded-scan test).
        fake = FakeGitHub([("success", "skipped")] + delivered_outage(130))
        self.assertIsNone(self.decide(fake, "failure"))
        self.assertEqual(self.decide(fake, "success"), "recovery")
        self.assertGreaterEqual(fake.runs_calls, 2,
                                "expected paginated reconstruction past one page")

    def test_previous_outcomes_reach_the_delivered_opening_alert(self):
        fake = FakeGitHub([("success", "skipped")] + delivered_outage(14))
        with mock.patch.object(history, "_get", side_effect=fake):
            outcomes = history.previous_probe_outcomes(fake.env("failure"), ENV_NAME)
        self.assertIn(("failure", "success"), outcomes,
                      "the delivered opening alert fell out of reconstructed history")
        self.assertGreaterEqual(len(outcomes), 14)

    # -- delivery evidence at both ends of the retained window --

    def test_delivery_at_oldest_retained_failure_is_honoured(self):
        # Newest-first retained window whose oldest failure is the delivered
        # opening alert (everything older has rolled off): that delivery
        # evidence must not be discarded just for sitting at the edge.
        previous = [("failure", "skipped")] * 9 + [("failure", "success")]
        self.assertIsNone(history.transition("failure", previous))
        self.assertEqual(history.transition("success", previous), "recovery")

    def test_delivery_at_newest_failure_is_honoured(self):
        # Retry delivered only on the most recent failure.
        previous = [("failure", "success")] + [("failure", "failure")] * 11
        self.assertIsNone(history.transition("failure", previous))
        self.assertEqual(history.transition("success", previous), "recovery")

    # -- delivery failure / retry across the window --

    def test_failed_opening_delivery_retried_until_it_lands(self):
        samples = [("success", "skipped"), ("failure", "skipped"),
                   ("failure", "failure"), ("failure", "success")]
        samples += [("failure", "skipped")] * 12
        fake = FakeGitHub(samples)
        self.assertIsNone(self.decide(fake, "failure"),
                          "the retried delivery landed; no further alert is due")
        self.assertEqual(self.decide(fake, "success"), "recovery")

    def test_never_delivered_outage_still_alerts_and_never_recovers(self):
        samples = [("success", "skipped"), ("failure", "skipped")]
        samples += [("failure", "failure")] * 13
        fake = FakeGitHub(samples)
        self.assertEqual(self.decide(fake, "failure"), "alert",
                         "an outage nobody was told about must keep retrying")
        self.assertEqual(self.decide(fake, "success"), "alert",
                         "the late alert is due on green; a recovery would be a lie")

    # -- skipped / cancelled / no-op steps cannot move state --

    def test_skipped_and_cancelled_runs_do_not_reset_or_close_outage(self):
        samples = [("success", "skipped")] + delivered_outage(12)
        noise = [{"sample": ("cancelled", "skipped")},
                 {"sample": ("skipped", "skipped")},
                 {"event": "workflow_dispatch", "sample": ("success", "success")}]
        fake = FakeGitHub(samples, extra_runs=noise)
        self.assertIsNone(self.decide(fake, "failure"))
        self.assertEqual(self.decide(fake, "success"), "recovery",
                         "cancelled/skipped/dispatch runs must not fake the recovery away")

    def test_skipped_post_step_never_counts_as_delivery(self):
        # Twelve failures where the post step never ran: nobody was alerted,
        # so the next failure alerts and a green run owes the late alert.
        previous = [("failure", "skipped")] * 12
        self.assertEqual(history.transition("failure", previous), "alert")
        self.assertEqual(history.transition("success", previous), "alert")

    # -- incomplete / unknown history: fail open, never invent state --

    def test_history_read_failure_mid_pagination_preserves_fail_open(self):
        # Page 1 reads fine, the next page errors: reconstruction is
        # unknown, so the documented policy applies verbatim - alert on a
        # real failure, stay silently green on a healthy sample. The
        # outage exceeds one capped page, so no implementation can know
        # the delivered state from page 1 alone.
        fake = FakeGitHub([("success", "skipped")] + delivered_outage(130),
                          fail_page=2)
        self.assertEqual(self.decide(fake, "failure"), "alert")
        self.assertIsNone(self.decide(fake, "success"),
                          "unknown history on green must not manufacture alert/recovery")

    def test_history_read_failure_on_first_page_preserves_fail_open(self):
        fake = FakeGitHub(delivered_outage(14), fail_first=True)
        self.assertEqual(self.decide(fake, "failure"), "alert")
        self.assertIsNone(self.decide(fake, "success"))

    # -- reconstruction is bounded, not an unbounded scan --

    def test_reconstruction_stops_at_the_incident_boundary(self):
        # 300 healthy samples precede the outage. Correct reconstruction
        # needs the outage plus its healthy boundary, not the whole past:
        # it must not read jobs for hundreds of pre-incident runs.
        samples = [("success", "skipped")] * 300 + delivered_outage(45)
        fake = FakeGitHub(samples)
        self.assertIsNone(self.decide(fake, "failure"))
        self.assertLess(fake.jobs_calls, 150,
                        f"reconstruction scanned {fake.jobs_calls} runs; "
                        "it must stop once the incident boundary is known")

    # -- a recovered incident stays recovered across the window --

    def test_recovered_incident_does_not_recover_twice_after_rollover(self):
        samples = [("success", "skipped")] + delivered_outage(12)
        samples += [("success", "success")]  # recovery post delivered
        samples += [("success", "skipped")] * 12
        fake = FakeGitHub(samples)
        self.assertIsNone(self.decide(fake, "success"),
                          "recovery already delivered; a second one is a duplicate")
        self.assertIsNone(self.decide(fake, "failure"),
                          "a single new failure is not yet an incident")


if __name__ == "__main__":
    unittest.main()

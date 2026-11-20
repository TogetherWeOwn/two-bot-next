"""Standalone classification checks adapted from the diagnostic draft."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import incident_issue
import production_hold_evidence as hold


def job(name=hold.PRODUCTION_JOB, status="completed", conclusion="success"):
    return {"name": name, "status": status, "conclusion": conclusion}


class EvidenceClassificationTests(unittest.TestCase):
    def test_malformed_job_fields_fail_before_wait_classification(self):
        valid = job(status="waiting", conclusion=None)
        malformed = [
            {"status": "waiting", "conclusion": None},
            job(name=None, status="waiting", conclusion=None),
            job(name=42, status="waiting", conclusion=None),
            {"name": hold.PRODUCTION_JOB, "conclusion": None},
            job(status=None, conclusion=None),
            job(status="unknown", conclusion=None),
            {"name": hold.PRODUCTION_JOB, "status": "waiting"},
            job(status="completed", conclusion=None),
            job(conclusion=42),
        ]
        for row in malformed:
            with self.subTest(row=row):
                with self.assertRaisesRegex(incident_issue.IncidentError,
                                            "^production hold active job is malformed$"):
                    hold.classify_jobs([valid, row])
                self.assertEqual(hold.classify_jobs([valid])[1], "production waiting")

    def test_unfinished_jobs_with_conclusions_fail_before_classification(self):
        valid = job(status="waiting", conclusion=None)
        for status in hold.ACTIVE_STATES:
            for name in (hold.PRODUCTION_JOB, "other"):
                contradictory = job(name, status, "failure")
                for rows in ([contradictory], [valid, contradictory]):
                    with self.subTest(status=status, name=name, rows=rows):
                        with self.assertRaisesRegex(incident_issue.IncidentError,
                                                    "^production hold active job is malformed$"):
                            hold.classify_jobs(rows)

    def test_unsuccessful_terminal_outcomes_are_not_skipped(self):
        for conclusion in ("failure", "cancelled", "timed_out", "action_required",
                           "startup_failure", "stale"):
            with self.subTest(conclusion=conclusion):
                terminal = hold.classify_jobs([job(conclusion=conclusion)])
                self.assertIn("gate not green", terminal[1])
                active = hold.classify_jobs([job(status="waiting", conclusion=None)])
                self.assertEqual(active[1], "production waiting")
                self.assertFalse(terminal[0])
                self.assertTrue(active[0])
        self.assertIn("skipped or pending", hold.classify_jobs([job(conclusion="skipped")])[1])

    def test_all_active_states_and_both_target_jobs(self):
        for name in (hold.PRODUCTION_JOB, hold.ROLLBACK_JOB):
            for status in ("waiting", "queued", "pending", "requested", "in_progress"):
                with self.subTest(name=name, status=status):
                    self.assertEqual(hold.classify_jobs([job(conclusion="failure"),
                                                        job(name, status, None)]),
                                     (True, f"production {status}"))

    def test_valid_fallback_and_unrelated_active_jobs(self):
        for conclusion in ("success", "neutral", "skipped"):
            with self.subTest(conclusion=conclusion):
                self.assertEqual(hold.classify_jobs([job(conclusion=conclusion)]),
                                 (False, "production skipped or pending - inspect promotion run"))
        self.assertEqual(hold.classify_jobs([]),
                         (False, "production skipped or pending - inspect promotion run"))
        self.assertEqual(hold.classify_jobs([job("other", "waiting", None)])[0], False)

    def test_non_list_and_non_mapping_jobs_fail_closed(self):
        for jobs in (None, {}, [None], [42]):
            with self.subTest(jobs=jobs), self.assertRaises(incident_issue.IncidentError):
                hold.classify_jobs(jobs)

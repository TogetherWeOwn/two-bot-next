"""Standalone batch validation and inert import checks."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import incident_issue
import production_hold_evidence as evidence
from test_production_hold_import_probe import assert_inert_import


class EvidenceBatchTests(unittest.TestCase):
    def test_batch_shape_and_bounds(self):
        for value in (None, [], {}, {"workflow_runs": None},
                      {"workflow_runs": {}}, {"workflow_runs": [None] * 21}):
            with self.subTest(value=value), self.assertRaises(incident_issue.IncidentError):
                evidence.active_runs(value, "10")
        self.assertEqual(evidence.active_runs({"workflow_runs": []}, "10"), [])
        rows = [{"id": n, "run_attempt": 1} for n in range(1, 21)]
        self.assertEqual(evidence.active_runs({"workflow_runs": rows}, "10"),
                         rows[:9] + rows[10:])

    def test_ids_and_attempts_are_positive_integers(self):
        for value in (None, True, False, 0, -1, 1.5, "1"):
            for field in ("id", "run_attempt"):
                row = {"id": 1, "run_attempt": 1, field: value}
                with self.subTest(field=field, value=value), self.assertRaises(incident_issue.IncidentError):
                    evidence.active_runs({"workflow_runs": [row]}, "10")
        for row in (None, {}, {"id": 1}):
            with self.subTest(row=row), self.assertRaises(incident_issue.IncidentError):
                evidence.active_runs({"workflow_runs": [row]}, "10")

    def test_current_run_excluded_before_attempt_validation(self):
        rows = [{"id": 4, "run_attempt": 2}, {"id": 10, "run_attempt": False},
                {"id": 2, "run_attempt": 3}]
        self.assertEqual(evidence.active_runs({"workflow_runs": rows}, "10"), [rows[0], rows[2]])
        with self.assertRaises(incident_issue.IncidentError):
            evidence.active_runs({"workflow_runs": [rows[0], {"id": 2}]}, "10")

    def test_import_is_inert(self):
        assert_inert_import(self, "production_hold_evidence",
                            ["production_hold_evidence", "incident_issue"],
                            ["production_hold_evidence", "incident_issue"])

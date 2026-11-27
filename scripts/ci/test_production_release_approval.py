"""Deterministic attendance approval history fixtures."""

from copy import deepcopy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from production_release_approval import require_unused_attendance_plan
from rollback_proof import ProofError
from staging_rollout import GateError

PRODUCER = {"run_id": 101, "artifact_id": 901, "archive_digest": "sha256:" + "d" * 64}
RUNS = ("actions/workflows/deploy-production.yml/runs?event=workflow_dispatch&branch=main"
        "&created=%3E%3D2026-10-01T00%3A00%3A00Z&per_page=30")
JOBS = "actions/runs/202/attempts/1/jobs?per_page=30"
REPO = {"full_name": "TogetherWeOwn/two-bot-next", "id": 1396224242}


def run(run_id, minute, **overrides):
    return {
        "id": run_id, "run_attempt": 1, "path": ".github/workflows/deploy-production.yml@main",
        "event": "workflow_dispatch", "head_branch": "main", "head_sha": "a" * 40,
        "repository": REPO, "head_repository": REPO, "created_at": f"2026-10-01T00:{minute:02}:00Z",
        "display_title": "attendance release plan 101", "status": "in_progress", "conclusion": None,
        **overrides,
    }


def job(job_id, name, conclusion):
    return {"id": job_id, "name": name, "conclusion": conclusion, "status": "completed",
            "run_id": 202, "run_attempt": 1, "head_sha": "a" * 40}


class AttendanceApprovalHistoryTests(unittest.TestCase):
    def setUp(self):
        self.current = run(203, 10)
        self.producing_run = run(101, 0, path=".github/workflows/production-migrate.yml", status="completed",
                                 conclusion="success", display_title="production-migrate")
        self.api_rows = {"actions/runs/101": self.producing_run}
        self.history(self.current)

    def history(self, *rows):
        self.api_rows[RUNS] = {"total_count": len(rows), "workflow_runs": list(rows)}

    def api(self, path):
        return deepcopy(self.api_rows[path])

    def verify(self, producer=PRODUCER, current=None):
        return require_unused_attendance_plan(self.api, producer, self.current if current is None else current)

    def test_fresh_attempt_accepts_explicit_other_producer_and_ordinary_history(self):
        legacy = run(202, 5, display_title="deploy-production", status="completed")
        self.history(run(201, 4, display_title="attendance release plan 100", status="completed", conclusion="failure"),
                     self.current, legacy)
        self.api_rows[JOBS] = {"total_count": 2, "jobs": [
            job(501, "guard", "success"), job(502, "Production release / Prepare and stop the prior pair", "skipped"),
        ]}
        self.assertIsNone(self.verify())
        legacy["display_title"] = "attendance release plan 0101"
        self.assertIsNone(self.verify())
        self.api_rows[JOBS]["jobs"][-1]["conclusion"] = "success"
        with self.assertRaisesRegex(GateError, "superseded_by_release_attempt"):
            self.verify()

    def test_same_producer_is_consumed_even_by_failed_or_uncertain_attempt(self):
        for status, conclusion, attempt, minute in (("completed", "success", 1, 5), ("completed", "failure", 2, 5),
                                                     ("completed", "cancelled", 1, 5), ("in_progress", None, 1, 5),
                                                     ("queued", None, 1, 10)):
            with self.subTest(status=status, conclusion=conclusion, attempt=attempt, minute=minute):
                self.history(self.current, run(202, minute, status=status, conclusion=conclusion, run_attempt=attempt))
                with self.assertRaisesRegex(GateError, "superseded_by_release_attempt"):
                    self.verify()

    def test_unstamped_release_job_is_ambiguous(self):
        self.history(run(202, 5, display_title="deploy-production", status="completed"), self.current)
        for conclusion in ("success", "failure", "cancelled", None):
            with self.subTest(conclusion=conclusion):
                self.api_rows[JOBS] = {"total_count": 1, "jobs": [
                    job(501, "Production release / Apply the approved schema", conclusion),
                ]}
                with self.assertRaises(GateError):
                    self.verify()
        self.api_rows[JOBS] = {"total_count": 1, "jobs": [dict(job(501, "guard", "success"), run_id=200)]}
        with self.assertRaises(GateError):
            self.verify()

    def test_refuses_unstamped_rerun_with_skipped_first_attempt(self):
        self.history(self.current, run(202, 5, display_title="deploy-production", status="completed", run_attempt=2))
        self.api_rows[JOBS] = {"total_count": 1, "jobs": [
            job(501, "Production release / Prepare and stop the prior pair", "skipped"),
        ]}
        with self.assertRaisesRegex(GateError, "production_attendance_history_incomplete"):
            self.verify()

    def test_refuses_unproved_legacy_job_inventory(self):
        legacy = run(202, 5, display_title="deploy-production", status="completed")
        guard = job(501, "guard", "success")
        self.history(self.current, {**legacy, "status": "in_progress"})
        with self.assertRaises(GateError):
            self.verify()

        self.history(self.current, legacy)
        for jobs, total in (([], 0), ([guard], 2), ([guard, guard], 2),
                            ([job(i, "ordinary job", "success") for i in range(501, 532)], 31)):
            with self.subTest(rows=len(jobs), total=total), self.assertRaises(GateError):
                self.api_rows[JOBS] = {"total_count": total, "jobs": jobs}
                self.verify()

    def test_legacy_job_attempt_is_optional_but_must_match_if_present(self):
        self.history(self.current, run(202, 5, display_title="deploy-production", status="completed"))
        row = job(501, "Production release / Prepare and stop the prior pair", "skipped")
        del row["run_attempt"]
        self.api_rows[JOBS] = {"total_count": 1, "jobs": [row]}
        self.assertIsNone(self.verify())

        for attempt in (2, True, "1", None):
            with self.subTest(attempt=attempt), self.assertRaises(GateError):
                row["run_attempt"] = attempt
                self.verify()

    def test_refuses_wrong_producer_or_current_attempt(self):
        for fields in ({"artifact_id": True}, {"archive_digest": "a" * 64}, {"run_id": False}):
            with self.subTest(producer=fields), self.assertRaises(GateError):
                self.verify({**PRODUCER, **fields})
        for fields in ({"id": 101}, {"run_attempt": 2}, {"path": ".github/workflows/deploy-production.yml@preview"},
                       {"head_branch": "preview"}, {"display_title": "attendance release plan 102"},
                       {"created_at": "2026-09-30T23:59:59Z"}, {"status": "completed"}):
            with self.subTest(current=fields), self.assertRaises(GateError):
                self.verify(current={**self.current, **fields})
        for fields in ({"id": 102}, {"run_attempt": True}, {"created_at": "2026-02-31T00:00:00Z"},
                       {"head_repository": {**REPO, "id": 2}}, {"conclusion": "failure"}):
            with self.subTest(producing_run=fields), self.assertRaises(GateError):
                self.api_rows["actions/runs/101"] = {**self.producing_run, **fields}
                self.verify()

    def test_refuses_unverified_history(self):
        for fields in ({"display_title": None}, {"display_title": "different title"},
                       {"head_repository": {**REPO, "full_name": "another/repo"}},
                       {"path": ".github/workflows/another.yml"}, {"event": "push"}):
            with self.subTest(prior=fields), self.assertRaises(GateError):
                self.history(self.current, run(202, 6, **fields))
                self.verify()
        for rows in ([], [self.current, self.current], [{**self.current, "head_sha": "b" * 40}],
                     [{**self.current, "created_at": "2026-10-01T00:11:00Z"}],
                     [self.current, *[run(i, 4, display_title="attendance release plan 100") for i in range(300, 330)]]):
            with self.subTest(row_count=len(rows)), self.assertRaises(GateError):
                self.history(*rows)
                self.verify()
        for count in (True, 31):
            with self.subTest(count=count), self.assertRaises(GateError):
                self.history(self.current)
                self.api_rows[RUNS]["total_count"] = count
                self.verify()

    def test_history_transport_failure_propagates(self):
        def unavailable(path):
            if path == RUNS:
                raise ProofError("github_transport_failure")
            return self.api(path)

        with self.assertRaisesRegex(ProofError, "github_transport_failure"):
            require_unused_attendance_plan(unavailable, PRODUCER, self.current)

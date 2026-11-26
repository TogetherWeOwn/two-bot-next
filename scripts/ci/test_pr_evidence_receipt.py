"""Offline fixtures for the dormant PR description receipt contract."""

from copy import deepcopy
from dataclasses import replace
import hashlib
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github/scripts"))
import pr_evidence as evidence
from pr_evidence_receipt import ReceiptAttempt, issue_receipt, matches_receipt


REPO, BASE, HEAD = "example/repo", "a" * 40, "b" * 40
IDENTITY = (1, REPO, BASE, HEAD)
ATTEMPT = ReceiptAttempt(repo_id=7, workflow_id=8, workflow_sha="c" * 40, run_id=10, attempt=2, job_id=12)
BODY = "cafe\u0301\r\n<!-- synthetic footer -->\r\n"
PR = {"number": 1, "state": "open", "base": {"repo": {"full_name": REPO}, "sha": BASE},
      "head": {"sha": HEAD}, "body": BODY}


class ReceiptTests(unittest.TestCase):
    def setUp(self):
        self.reader = Mock()
        self.reader.get.return_value = PR
        self.policy = object()
        self.enterContext(patch.object(evidence, "sources", return_value=[]))
        self.validate = self.enterContext(patch.object(evidence, "validate"))

    def issue(self, attempt=ATTEMPT):
        return issue_receipt(self.policy, self.reader, *IDENTITY, attempt=attempt)

    def test_receipt_hashes_the_description_the_verifier_rechecked(self):
        receipt = self.issue()
        self.assertEqual(receipt["description_sha256"], hashlib.sha256(BODY.encode("utf-8")).hexdigest())
        self.assertEqual(self.validate.call_args.args[1], BODY)
        self.assertEqual(self.reader.get.call_count, 2)
        self.assertNotIn("synthetic footer", json.dumps(receipt))

        self.reader.get.return_value = dict(PR, body=None)
        self.assertEqual(self.issue()["description_sha256"], hashlib.sha256(b"").hexdigest())
        self.assertEqual(self.validate.call_args.args[1], "")

    def test_receipt_refuses_validation_failure_drift_and_missing_identity(self):
        with self.assertRaises(evidence.EvidenceError):
            self.issue(None)
        self.reader.get.assert_not_called()
        for change in ({"job_id": True}, {"attempt": 0}, {"workflow_sha": ""}):
            with self.subTest(change=change), self.assertRaises(evidence.EvidenceError):
                replace(ATTEMPT, **change)
        self.validate.side_effect = evidence.EvidenceError("Rejected synthetic evidence.")
        with self.assertRaisesRegex(evidence.EvidenceError, "Rejected synthetic evidence"):
            self.issue()
        self.validate.side_effect = None
        for change in ({"body": "Edited at the same head"}, {"number": 2}, {"head": {"sha": "d" * 40}}):
            with self.subTest(change=change), self.assertRaises(evidence.EvidenceError):
                self.reader.get.side_effect = [PR, dict(PR, **change)]
                self.issue()

    def test_parser_requires_the_exact_protected_attempt_and_description(self):
        complete = {"status": "completed", "conclusion": "success"}
        association = {"number": 1, "base": {"repo": {"id": 7}, "sha": BASE}, "head": {"sha": HEAD}}
        run = dict(complete, id=10, run_attempt=2, workflow_id=8, path=".github/workflows/pr-evidence.yml",
                   event="pull_request", head_sha=HEAD, repository={"full_name": REPO, "id": 7},
                   pull_requests=[association])
        step = dict(complete, name="Verify and publish evidence")
        job = dict(complete, name="pr-evidence", id=12, run_id=10, run_attempt=2, head_sha=HEAD, steps=[step])
        listing = {"total_count": 1, "jobs": [job]}
        receipt = json.loads(json.dumps(self.issue()))
        verdict, _ = evidence.current_snapshot(self.reader, *IDENTITY)

        def match(record=receipt, snapshot=verdict, run_record=run, jobs=listing, attempt=ATTEMPT, origin=12):
            return matches_receipt(record, snapshot, run_record, jobs, attempt=attempt, receipt_job_id=origin)

        self.assertTrue(match())
        self.assertFalse(match(attempt=None))
        self.assertFalse(match(attempt=replace(ATTEMPT, workflow_sha="e" * 40)))
        self.assertFalse(match(origin=None))
        self.assertFalse(match(origin=11))
        self.reader.get.return_value = dict(PR, body=BODY.replace("\r\n", "\n"))
        edited, _ = evidence.current_snapshot(self.reader, *IDENTITY)
        self.assertFalse(match(snapshot=edited))

        for change in ({"version": True}, {"number": 2}, {"attempt": 1}, {"base": HEAD}, {"head": BASE},
                       {"repo_id": 9}, {"workflow_id": 9}, {"run_id": 9}, {"job_id": 11}, {"body": BODY}):
            with self.subTest(receipt=change):
                self.assertFalse(match(record=dict(receipt, **change)))
        for change in ({"pull_requests": []}, {"pull_requests": [dict(association, number=True)]},
                       {"repository": {"full_name": REPO, "id": 9}}, {"workflow_id": 9},
                       {"pull_requests": [dict(association, base={"repo": {"id": 7}, "sha": HEAD})]},
                       {"path": ".github/workflows/other.yml"}, {"event": "push"}, {"run_attempt": 1},
                       {"status": "in_progress"}, {"conclusion": "failure"}, {"head_sha": BASE}):
            with self.subTest(run=change):
                self.assertFalse(match(run_record=dict(run, **change)))
        for change in ({"id": 11}, {"run_attempt": 1}, {"head_sha": BASE}, {"conclusion": "cancelled"},
                       {"steps": []}, {"steps": [step, step]}, {"steps": [dict(step, conclusion="failure")]}):
            with self.subTest(job=change):
                self.assertFalse(match(jobs=dict(listing, jobs=[dict(job, **change)])))

        for key in ("run_attempt", "id"):
            missing = deepcopy(listing)
            del missing["jobs"][0][key]
            self.assertFalse(match(jobs=missing))
        no_attempt = dict(run)
        del no_attempt["run_attempt"]
        self.assertFalse(match(run_record=no_attempt))

        first = replace(ATTEMPT, attempt=1)
        first_receipt = dict(receipt, attempt=1)
        first_run = dict(run, run_attempt=1)

        def match_first(run_record, job_record):
            return match(record=first_receipt, run_record=run_record,
                         jobs=dict(listing, jobs=[job_record]), attempt=first)

        self.assertTrue(match_first(first_run, dict(job, run_attempt=1)))
        without_job_attempt = dict(job)
        del without_job_attempt["run_attempt"]
        self.assertTrue(match_first(first_run, without_job_attempt))

        for run_record in (no_attempt, run, dict(first_run, id=11), dict(first_run, run_attempt=True),
                           dict(first_run, conclusion="failure"), dict(first_run, status="in_progress")):
            with self.subTest(first_run=run_record):
                self.assertFalse(match_first(run_record, without_job_attempt))
        for job_attempt in (None, True, "1", 2):
            with self.subTest(first_job_attempt=job_attempt):
                self.assertFalse(match_first(first_run, dict(job, run_attempt=job_attempt)))

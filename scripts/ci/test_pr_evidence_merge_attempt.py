"""Offline consumer of a trusted PR evidence verdict at a merge attempt."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import ANY, Mock, patch

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / ".github/scripts"))
import pr_evidence as evidence

REPO, BASE, HEAD = "example/repo", "a" * 40, "b" * 40
ORIGINAL = {"number": 1, "state": "open", "base": {"repo": {"full_name": REPO}, "sha": BASE, "ref": "main"},
            "head": {"sha": HEAD}, "body": "Original valid evidence"}
EDITED = dict(ORIGINAL, body="Revised valid evidence")
REHEARSAL = Path(__file__).with_name("rehearse_pr_evidence.py")
WORKFLOW_PATH = ".github/workflows/pr-evidence.yml"


def capture(pr, run_id, conclusion, attempt=1):
    completed = {"status": "completed", "conclusion": conclusion}
    run = dict(completed, id=run_id, repository={"full_name": REPO}, head_sha=HEAD, run_attempt=attempt,
               created_at=f"2026-11-25T14:00:{run_id}Z", event="pull_request", path=WORKFLOW_PATH,
               pull_requests=[{"number": pr["number"], "head": pr["head"]}])
    job = dict(completed, id=run_id * 10 + attempt, name="pr-evidence", run_id=run_id, run_attempt=attempt,
               head_sha=HEAD, steps=[dict(completed, name="Verify and publish evidence")])
    return {"pr": pr, "run": run, "jobs": {"total_count": 1, "jobs": [job]}}


class MergeAttemptTests(unittest.TestCase):
    def test_merge_attempt_requires_current_verdict(self):
        identity = (1, REPO, BASE, HEAD)
        reader, accept_merge, policy = Mock(), Mock(), object()
        reader.get.return_value = ORIGINAL
        # Isolate the consumer. The advisory fixtures exercise the actual evidence policy.
        self.enterContext(patch.object(evidence, "sources", return_value=[]))
        validate = self.enterContext(patch.object(evidence, "validate"))

        def attempt_merge(recorded):
            try:
                current = evidence.evaluate_current(policy, reader, *identity)
            except evidence.EvidenceError:
                return
            if recorded == current and recorded.matches_current(reader, *identity):
                accept_merge(recorded.head)

        prior = evidence.evaluate_current(policy, reader, *identity)
        for case, reads in (
                ("edit before validation", [EDITED, EDITED]),
                ("edit during validation", [ORIGINAL, EDITED]),
                ("edit after validation", [ORIGINAL, ORIGINAL, EDITED])):
            with self.subTest(case=case), patch.object(reader, "get", side_effect=reads) as get:
                attempt_merge(prior)
                self.assertEqual(get.call_count, len(reads))
                accept_merge.assert_not_called()

        reader.get.return_value = EDITED
        fresh = evidence.evaluate_current(policy, reader, *identity)
        self.assertEqual(prior.head, fresh.head)
        validate.reset_mock()
        attempt_merge(fresh)
        validate.assert_called_once_with(policy, EDITED["body"], [], ANY)
        accept_merge.assert_called_once_with(HEAD)


class OperatorRehearsalTests(unittest.TestCase):
    def rehearse(self, mode, *captures):
        with tempfile.TemporaryDirectory() as directory:
            paths = []
            for index, capture in enumerate(captures):
                path = Path(directory) / f"{index}.json"
                path.write_text(json.dumps(capture), encoding="utf-8")
                paths.append(str(path))
            return subprocess.run([sys.executable, "-B", str(REHEARSAL), mode, *paths],
                                  capture_output=True, text=True, timeout=10)

    def test_rehearsal_distinguishes_current_failure_and_stale_green(self):
        def with_job(record, job):
            return dict(record, jobs={"total_count": 1, "jobs": [job]})

        before = capture(ORIGINAL, 10, "success")
        failure = capture(EDITED, 11, "failure")
        job, = failure["jobs"]["jobs"]
        verify, = job["steps"]
        missing_attempt = with_job(failure, {key: value for key, value in job.items() if key != "run_attempt"})
        setup_failure = with_job(failure, dict(job, steps=[dict(verify, conclusion="skipped")]))
        later_failure = with_job(failure, dict(job, steps=[dict(verify, conclusion="success")]))
        cancelled = capture(EDITED, 11, "cancelled")
        cancelled_job, = cancelled["jobs"]["jobs"]
        cancelled = with_job(cancelled, dict(cancelled_job, steps=[dict(verify, conclusion="success")]))
        for current, code, output in (
                (failure, 1, "current_advisory=failure\nprior_hosted=stale_green\n"),
                (missing_attempt, 1, "current_advisory=failure\nprior_hosted=stale_green\n"),
                ({"pr": EDITED}, 2, "current_advisory=unavailable\nprior_hosted=stale_green\n"),
                (setup_failure, 2, "current_advisory=unavailable\nprior_hosted=stale_green\n"),
                (cancelled, 2, "current_advisory=unavailable\nprior_hosted=stale_green\n"),
                (later_failure, 1, "current_advisory=failed_after_verification\nprior_hosted=stale_green\n"),
                (capture(EDITED, 12, "success"), 0, "current_advisory=success\nprior_hosted=stale_green\n"),
                (capture(ORIGINAL, 13, "success"), 0, "current_advisory=success\nprior_hosted=green_same_description\n"),
                (with_job(failure, dict(job, run_attempt=2)), 2, ""),
                (dict(missing_attempt, run=dict(failure["run"], run_attempt=2)), 2, ""),
                (dict(failure, jobs={"total_count": 2, "jobs": [job]}), 2, ""),
                (dict(failure, run=dict(failure["run"], pull_requests=[])), 2, ""),
                (dict(failure, run=dict(failure["run"], created_at="2026-11-25T13:59:59Z")), 2, ""),
                ({"pr": dict(EDITED, head={"sha": "c" * 40})}, 2, ""),
                (dict(before, pr=EDITED), 2, "")):
            with self.subTest(current=current):
                result = self.rehearse("pr", before, current)
                self.assertEqual((result.returncode, result.stdout), (code, output), result.stderr)

    def test_late_verifier_cannot_restore_green_after_same_head_edits(self):
        invalid = dict(ORIGINAL, body="Invalid evidence")
        reader, policy = Mock(), object()
        reader.get.return_value = ORIGINAL
        self.enterContext(patch.object(evidence, "evidence_sections", return_value=()))
        self.enterContext(patch.object(evidence, "sources", return_value=[]))
        validate = self.enterContext(patch.object(evidence, "validate"))

        def verify():
            return evidence.check(policy, reader, 1, REPO, BASE, HEAD)

        passed = "Verification and Attestation passed"
        self.assertEqual(verify(), passed)

        def overlapping_edits(_policy, body, _files, _read):
            if body == invalid["body"]:
                raise evidence.EvidenceError("Synthetic evidence is invalid.")
            if body == ORIGINAL["body"]:
                # Let both edit callbacks finish before the original verifier's final read.
                reader.get.return_value = invalid
                with self.assertRaisesRegex(evidence.EvidenceError, "Synthetic evidence is invalid"):
                    verify()
                reader.get.return_value = EDITED
                self.assertEqual(verify(), passed)

        validate.side_effect = overlapping_edits
        with self.assertRaisesRegex(evidence.EvidenceError, "PR evidence changed during validation"):
            verify()
        self.assertEqual([call.args[1] for call in validate.call_args_list],
                         [ORIGINAL["body"], ORIGINAL["body"], invalid["body"], EDITED["body"]])

        before = capture(ORIGINAL, 10, "success")
        fresh = capture(EDITED, 13, "success")
        # A rerun can verify the live description, but its run ID and creation time stay fixed.
        rerun = capture(EDITED, 10, "success", attempt=2)
        automatic = self.rehearse("pr", before, fresh)
        self.assertEqual((automatic.returncode, automatic.stdout),
                         (0, "current_advisory=success\nprior_hosted=stale_green\n"), automatic.stderr)
        for case, current in (
                ("rerun", rerun),
                ("reused run ID with later timestamp",
                 dict(rerun, run=dict(rerun["run"], created_at=fresh["run"]["created_at"]))),
                ("distinct run ID with unchanged timestamp",
                 dict(fresh, run=dict(fresh["run"], created_at=before["run"]["created_at"])))):
            with self.subTest(case=case):
                result = self.rehearse("pr", before, current)
                self.assertEqual((result.returncode, result.stdout, result.stderr),
                                 (2, "", "Invalid or unmatched evidence capture.\n"))

    def test_description_edit_keeps_a_cancelling_verification_route(self):
        path = ROOT / WORKFLOW_PATH
        workflow = yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
        trigger = workflow["on"]["pull_request"]
        self.assertIn("edited", trigger["types"], "A description-only edit needs a fresh run without a push.")
        self.assertFalse({"paths", "paths-ignore"} & trigger.keys())
        self.assertEqual(workflow["concurrency"], {
            "group": "pr-evidence-${{ github.event_name == 'push' && 'main' || "
                     "github.event.pull_request.number || github.sha }}",
            "cancel-in-progress": "${{ github.event_name == 'pull_request' }}",
        })
        job = workflow["jobs"]["pr-evidence"]
        verify, = [step for step in job["steps"] if step.get("name") == "Verify and publish evidence"]
        self.assertNotIn("if", job)
        self.assertNotIn("if", verify)
        self.assertEqual(verify["run"], "python3 -I .github/scripts/pr_evidence.py verify")

    def test_rehearsal_refuses_unavailable_group_evidence(self):
        group = {"base_sha": BASE, "head_sha": HEAD, "base_ref": "refs/heads/main",
                 "head_ref": "refs/heads/gh-readonly-queue/main/pr-1-bbbbbbb"}
        result = self.rehearse("group", {"action": "checks_requested", "merge_group": group})
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "merge_group=unavailable (no authoritative PR membership)\n")

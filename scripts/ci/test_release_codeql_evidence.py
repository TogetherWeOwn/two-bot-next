"""Rehearse CodeQL upload failures through the reviewed-source CI reader offline."""

import sys
import unittest
from pathlib import Path
from urllib.parse import urlencode

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from staging_qualification import REPO, workflow_evidence
from staging_rollout import GateError

SHA = "a" * 40
OTHER_SHA = "b" * 40
WORKFLOW_ID = 72
RUN_ID = 101
SUITE_ID = 201
RUN = f"actions/runs/{RUN_ID}"
WORKFLOW_PATH = "dynamic/github-code-scanning/codeql"
REQUIRED = ("Analyze (actions)", "Analyze (javascript-typescript)", "Analyze (python)", "Analyze (rust)")
RUST = REQUIRED[-1]
SUCCESS = {"status": "completed", "conclusion": "success"}


class ReleaseCodeqlEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.routes = {}
        first = self.attempt(1)
        self.earlier_rust_check = self.routes[self.check_path(RUST)]["check_runs"][0]
        # The workflow listing can lag behind the current run read.
        self.routes[f"actions/workflows/{WORKFLOW_ID}/runs?head_sha={SHA}&branch=main"
                    "&event=dynamic&per_page=100&page=1"] = {"total_count": 1, "workflow_runs": [first]}

    def check_path(self, name):
        query = urlencode({"check_name": name, "app_id": 15368, "filter": "latest", "per_page": 100})
        return f"commits/{SHA}/check-runs?{query}"

    def attempt(self, number, *, upload_failed=False):
        run = {
            **SUCCESS, "id": RUN_ID, "run_number": 10, "run_attempt": number, "check_suite_id": SUITE_ID,
            "path": WORKFLOW_PATH, "event": "dynamic", "head_branch": "main", "head_sha": SHA,
            "repository": {"full_name": REPO}, "head_repository": {"full_name": REPO},
        }
        jobs = []
        for offset, name in enumerate(REQUIRED):
            job_id = number * 100 + offset
            url = f"https://api.github.com/repos/{REPO}/check-runs/{job_id}"
            row = {**SUCCESS, "id": job_id, "name": name, "head_sha": SHA}
            if upload_failed and name == RUST:
                # CodeQL's hosted analysis step also uploads the results. A failed
                # upload makes that job and its attempt fail even when other analyses finish.
                run["conclusion"] = row["conclusion"] = "failure"
            jobs.append({**row, "run_id": RUN_ID, "run_attempt": number, "check_run_url": url})
            self.routes[self.check_path(name)] = {"check_runs": [
                {**row, "url": url, "app": {"id": 15368}, "check_suite": {"id": SUITE_ID}},
            ]}
        self.routes[RUN] = run
        self.routes[self.jobs_path(number)] = {"total_count": len(jobs), "jobs": jobs}
        return run

    def jobs_path(self, number):
        return f"{RUN}/attempts/{number}/jobs?per_page=100&page=1"

    def read_evidence(self):
        return workflow_evidence(SHA, self.routes.__getitem__, workflow=WORKFLOW_ID,
                                 path=WORKFLOW_PATH, event="dynamic", checks=REQUIRED)

    def test_failed_upload_refuses_despite_earlier_success_and_accepts_exact_replacement(self):
        self.assertEqual(self.read_evidence(), {"run_id": RUN_ID, "attempt": 1})
        self.attempt(2, upload_failed=True)
        self.routes[self.check_path(RUST)]["check_runs"].append(self.earlier_rust_check)
        with self.assertRaisesRegex(GateError, "^candidate_ci_unproved$"):
            self.read_evidence()

        self.attempt(3)
        self.routes[self.check_path(RUST)]["check_runs"].append(self.earlier_rust_check)
        self.assertEqual(self.read_evidence(), {"run_id": RUN_ID, "attempt": 3})

    def test_earlier_rust_job_cannot_complete_a_partial_attempt(self):
        self.attempt(2)
        jobs = self.routes[self.jobs_path(2)]["jobs"]
        jobs[-1] = self.routes[self.jobs_path(1)]["jobs"][-1]
        self.routes[self.check_path(RUST)]["check_runs"].append(self.earlier_rust_check)
        with self.assertRaisesRegex(GateError, "^candidate_check_job_unproved$"):
            self.read_evidence()

    def test_only_the_exact_commit_check_can_replace_the_missing_result(self):
        self.attempt(2)
        checks = self.routes[self.check_path(RUST)]["check_runs"]
        exact = checks.pop()
        checks.append(self.earlier_rust_check)
        with self.assertRaisesRegex(GateError, "^candidate_check_unproved$"):
            self.read_evidence()

        # Even a positive row at the current job's URL cannot vouch for another SHA.
        checks.append({**exact, "head_sha": OTHER_SHA})
        with self.assertRaisesRegex(GateError, "^candidate_check_unproved$"):
            self.read_evidence()

        checks[-1] = exact
        self.assertEqual(self.read_evidence(), {"run_id": RUN_ID, "attempt": 2})

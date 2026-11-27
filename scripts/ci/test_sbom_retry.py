"""Prove Check's producer-rooted SBOM retry with synthetic GitHub inventories."""

from copy import deepcopy
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

import yaml

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import plan_sbom_retry as retry
sys.path.pop(0)

ROOT = SCRIPTS.parent
HEAD = "a" * 40
PR = 17
RUN = 25
ATTEMPT = 2
PIN = dict(number=PR, head=HEAD, run_id=RUN, attempt=ATTEMPT)


def inventory(key, *items):
    return {"total_count": len(items), key: list(items)}


def artifact(attempt, run=RUN):
    return {"name": f"sbom-partial-{run}-{attempt}", "workflow_run": {"id": run, "head_sha": HEAD}}


def workflow(name):
    return yaml.safe_load((ROOT / ".github/workflows" / name).read_text(encoding="utf-8"))["jobs"]


def dependents(edges, root):
    result = {root}
    while True:
        expanded = result | {job for job, needs in edges.items() if result.intersection(needs)}
        if expanded == result:
            return result
        result = expanded


class SbomRetryTests(unittest.TestCase):
    def setUp(self):
        repo = {"full_name": retry.REPO}
        self.pr = {"number": PR, "state": "open", "base": {"ref": "main", "repo": repo},
                   "head": {"ref": "fix/example", "repo": repo, "sha": HEAD}}
        self.run = {"event": "pull_request", "path": retry.CHECK, "repository": repo,
                    "head_repository": repo, "head_branch": "fix/example", "head_sha": HEAD,
                    "id": RUN, "run_number": 10, "run_attempt": ATTEMPT,
                    "pull_requests": [{"number": PR, "head": {"sha": HEAD}}],
                    "status": "completed", "conclusion": "failure"}
        self.runs = inventory("workflow_runs", self.run)
        jobs = [(retry.PRODUCER, "success", 1), (retry.VERIFIER, "failure", 2), ("ci-ok", "failure", 2),
                ("ci-evidence", "success", 2), ("check", "success", 1), ("patch coverage", "success", 2),
                ("worker check", "skipped", 1)]
        self.jobs = inventory("jobs", *[
            {"id": i, "name": name, "run_id": RUN, "head_sha": HEAD, "run_attempt": attempt,
             "status": "completed", "conclusion": conclusion}
            for i, (name, conclusion, attempt) in enumerate(jobs, 100)
        ])
        self.producer, self.verifier, *_ = self.jobs["jobs"]
        self.verifier["steps"] = [{"name": retry.DOWNLOAD_STEP + "pinned-revision", "status": "completed",
                                   "conclusion": "failure"}]
        self.artifacts = inventory("artifacts", artifact(1))

    def plan(self, **pin):
        return retry.plan(self.pr, self.runs, self.jobs, self.artifacts, **(PIN | pin))

    def test_retained_producer_requires_producer_rooted_retry(self):
        plan = self.plan()
        self.assertEqual(plan["producer_job_id"], self.producer["id"])
        self.assertNotEqual(plan["producer_job_id"], self.verifier["id"])
        self.assertEqual(plan["dependent_jobs"], (retry.VERIFIER, "ci-ok", "ci-evidence"))
        self.assertEqual(plan["expected_artifact"], f"sbom-partial-{RUN}-3")
        # GitHub can present the retained success as part of the current attempt.
        self.producer["run_attempt"] = ATTEMPT
        self.assertEqual(self.plan(), plan)

    def test_current_attempt_artifact_refuses_missing_handoff_recovery(self):
        self.artifacts = inventory("artifacts", artifact(ATTEMPT))
        with self.assertRaisesRegex(retry.Refusal, "Current-attempt artifact already exists"):
            self.plan()
        self.artifacts = inventory("artifacts", artifact(ATTEMPT + 1))
        with self.assertRaisesRegex(retry.Refusal, "Next-attempt artifact already exists"):
            self.plan()

    def test_refuses_wrong_attempt_or_failure_after_download(self):
        self.verifier["run_attempt"] = ATTEMPT - 1
        with self.assertRaisesRegex(retry.Refusal, "failed current-attempt verifier"):
            self.plan()
        self.verifier["run_attempt"] = ATTEMPT
        self.verifier["steps"][0]["conclusion"] = "success"
        with self.assertRaisesRegex(retry.Refusal, "Verifier did not fail at artifact download"):
            self.plan()

    def test_other_run_artifact_inventory_refuses(self):
        self.artifacts = inventory("artifacts", artifact(ATTEMPT, run=RUN + 1))
        with self.assertRaisesRegex(retry.Refusal, "Artifact inventory belongs to another run or head"):
            self.plan()

    def test_active_and_newer_runs_refuse(self):
        self.run["status"] = "in_progress"
        with self.assertRaisesRegex(retry.Refusal, "exact-head Check run is active"):
            self.plan()
        self.run["status"] = "completed"
        successor = self.run | {"id": RUN + 1, "run_number": 11, "status": "queued"}
        self.runs = inventory("workflow_runs", self.run, successor)
        with self.assertRaisesRegex(retry.Refusal, "exact-head Check run is active"):
            self.plan()
        successor["status"] = "completed"
        with self.assertRaisesRegex(retry.Refusal, "newer exact-head Check run exists"):
            self.plan()

    def test_stale_head_and_attempt_refuse(self):
        with self.assertRaisesRegex(retry.Refusal, "PR head changed"):
            self.plan(head="b" * 40)
        with self.assertRaisesRegex(retry.Refusal, "Check attempt changed"):
            self.plan(attempt=ATTEMPT + 1)

    def test_refuses_incomplete_unrelated_or_ambiguous_jobs(self):
        original = deepcopy(self.jobs)
        self.jobs["total_count"] += 1
        with self.assertRaisesRegex(retry.Refusal, "Incomplete jobs inventory"):
            self.plan()
        self.jobs = deepcopy(original)
        self.jobs["jobs"][5]["conclusion"] = "cancelled"
        with self.assertRaisesRegex(retry.Refusal, "unrelated Check job did not pass"):
            self.plan()
        self.jobs = original
        self.jobs["jobs"].append(self.producer | {"id": 200})
        self.jobs["total_count"] += 1
        with self.assertRaisesRegex(retry.Refusal, "Ambiguous SBOM retry job"):
            self.plan()

    def test_workflow_graph_replays_exact_attempt_handoff(self):
        check = workflow("check.yml")
        sbom = workflow("sbom.yml")
        caller = check.pop("supply-chain")
        self.assertEqual(caller["uses"], "./.github/workflows/sbom.yml")
        names = {key: f"{caller['name']} / {job['name']}" for key, job in sbom.items()}
        self.assertEqual(names, {"image": retry.PRODUCER, "verify": retry.VERIFIER})

        def needs(job):
            value = job.get("needs", [])
            return [value] if isinstance(value, str) else value

        # A caller downstream of the reusable invocation waits on both of its jobs.
        edges = {name: [names[dependency] for dependency in needs(sbom[key])]
                 for key, name in names.items()}
        for key, job in check.items():
            edges[key] = [node for dep in needs(job)
                          for node in (names.values() if dep == "supply-chain" else [dep])]
        self.assertEqual(check["ci-ok"]["name"], "ci-ok")
        self.assertEqual(check["ci-evidence"].get("name", "ci-evidence"), "ci-evidence")

        upload = next(step for step in sbom["image"]["steps"]
                      if step.get("name") == "Hand off SBOMs and image to verification")
        download = next(step for step in sbom["verify"]["steps"] if step.get("uses", "").startswith("actions/download-artifact@"))
        expected = retry.HANDOFF.format(run="${{ github.run_id }}", attempt="${{ github.run_attempt }}")
        self.assertEqual(upload["with"]["name"], expected)
        self.assertEqual(download["with"], {"name": expected, "path": "sbom"})
        self.assertTrue(("Run " + download["uses"]).startswith(retry.DOWNLOAD_STEP))

        prior = retry.HANDOFF.format(run=RUN, attempt=1)
        plan = self.plan()
        for root, attempt, accepted in ((retry.VERIFIER, 2, False), (plan["producer_job"], 3, True)):
            selected = dependents(edges, root)
            if root == retry.PRODUCER:
                self.assertEqual(selected, {root, *plan["dependent_jobs"]})
            artifact_name = retry.HANDOFF.format(run=RUN, attempt=attempt)
            available = {prior}
            if retry.PRODUCER in selected:
                available.add(artifact_name)
            self.assertEqual(artifact_name in available, accepted)
        self.assertEqual(artifact_name, plan["expected_artifact"])

    def test_cli_emits_plan_and_refuses_duplicate_json_without_echo(self):
        with TemporaryDirectory() as directory:
            snapshot = Path(directory)
            for name, document in (("pr", self.pr), ("runs", self.runs), ("jobs", self.jobs),
                                   ("artifacts", self.artifacts)):
                (snapshot / f"{name}.json").write_text(json.dumps(document), encoding="utf-8")
            args = [sys.executable, str(SCRIPTS / "plan_sbom_retry.py"), "--pr", str(PR), "--head", HEAD,
                    "--run", str(RUN), "--attempt", str(ATTEMPT), directory]
            planned = subprocess.run(args, capture_output=True, text=True, timeout=5, check=True)
            self.assertEqual(json.loads(planned.stdout)["producer_job_id"], self.producer["id"])
            (snapshot / "jobs.json").write_text('{"synthetic-marker": 1, "synthetic-marker": 2}', encoding="utf-8")
            refused = subprocess.run(args, capture_output=True, text=True, timeout=5, check=False)
            self.assertEqual(refused.returncode, 1)
            self.assertEqual(refused.stdout, "")
            self.assertEqual(refused.stderr, "SBOM retry plan refused: Invalid or oversized jobs snapshot\n")

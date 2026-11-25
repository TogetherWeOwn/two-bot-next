"""Exercise reviewed-source authorization with disposable Git and Actions evidence."""

from copy import deepcopy
from hashlib import sha256
import json
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from urllib.parse import parse_qs, urlsplit

from test_staging_bootstrap import recovery_plan_fixture, reviewed_plan_output
from test_staging_recovery_status import archive_fixture, github_fixture

import staging_qualification as qualification
from staging_recovery_status import projection_hash, store_projection_hash
from staging_rollout import GateError


class ReviewedStagingTests(unittest.TestCase):
    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", *args],
            stderr=subprocess.DEVNULL, text=True, timeout=10,
        ).strip()

    def commit(self, *names):
        for name in names:
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(self.git("rev-parse", "--short", "HEAD") if path.is_file() else name)
        self.git("add", *names)
        self.git("commit", "-qm", names[0])
        return self.git("rev-parse", "HEAD")

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name)
        self.git("init", "-qb", "main")
        self.prior = self.commit(".github/workflows/deploy-staging.yml")
        self.source = self.commit("candidate.rs")
        self.workflow = self.commit("unrelated.rs")
        self.tip = self.commit("wrangler/scripts/staging-voice-move-acceptance.mjs",
                               "wrangler/test/fixtures/staging-voice-move.ts",
                               "wrangler/test/staging-voice-move-acceptance.test.ts")
        self.git("switch", "-qc", "outside", self.prior)
        self.off_main = self.commit("outside.rs")
        self.git("switch", "--detach", self.source)
        self.workdir = self.repo / "wrangler"
        self.workdir.mkdir()
        self.lockfile = {"packages": {"node_modules/wrangler": {"version": qualification.WRANGLER}}}

        plan = {**recovery_plan_fixture(), "source_sha": self.source}
        plan.update(plan_manifest_sha256=projection_hash(plan), store_plan_manifest_sha256=store_projection_hash(plan))
        self.fresh = json.loads(reviewed_plan_output(self.workdir, plan, self.source)["plan"])
        self.routes = github_fixture()
        archive = archive_fixture(json.dumps(plan))
        digest = "sha256:" + sha256(archive).hexdigest()
        self.routes["actions/artifacts/17/zip"] = archive
        self.routes["actions/runs/13/artifacts?per_page=2"]["artifacts"][0].update(
            digest=digest, size_in_bytes=len(archive))
        pair = "11111111-1111-1111-1111-111111111111@sha256:" + "a" * 64
        self.inputs = {"sha": self.source, "reviewed_plan": f"13@{digest}", "recovery_mode": "verified",
                       "recovery_pair": pair}
        self.env = {
            "SOURCE_SHA": self.source, "GITHUB_SHA": self.workflow, "FETCHED_MAIN_SHA": self.tip,
            "REVIEWED_PLAN": self.inputs["reviewed_plan"], "RECOVERY_PAIR": pair, "RECOVERY_MODE": "verified",
            "GITHUB_REPOSITORY": qualification.REPO, "GITHUB_REPOSITORY_ID": "1396224242",
            "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_RUN_ID": "30", "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_REF": "refs/heads/main", "GITHUB_REF_PROTECTED": "true",
            "GITHUB_WORKFLOW_REF": f"{qualification.REPO}/{qualification.WORKFLOW}@refs/heads/main",
            "GITHUB_WORKFLOW_SHA": self.workflow,
        }
        repo = {"full_name": qualification.REPO}
        common = {"head_branch": "main", "repository": repo, "head_repository": repo,
                  "event": "workflow_dispatch", "path": qualification.WORKFLOW, "run_attempt": 1}
        self.current = {**common, "id": 30, "run_number": 10, "head_sha": self.workflow,
                        "display_title": qualification.REVIEWED_STAGING_TITLE + self.source}
        self.history = {**common, "id": 29, "run_number": 9, "head_sha": self.prior,
                        "status": "completed", "conclusion": "success"}
        self.routes["actions/runs/30"] = self.current
        self.routes["actions/workflows/deploy-staging.yml/runs?branch=main&status=success&per_page=1"] = {
            "workflow_runs": [self.history]}
        self.ci = {}
        self.jobs = {}
        self.checks = {}
        for run_id, (workflow, checks) in enumerate(qualification.CHECKS.items(), 51):
            self.ci[workflow] = {**common, "id": run_id, "run_number": 10, "head_sha": self.source,
                                 "check_suite_id": run_id, "event": "push",
                                 "path": f".github/workflows/{workflow}",
                                 "status": "completed", "conclusion": "success"}
            self.routes[f"actions/runs/{run_id}"] = self.ci[workflow]
            for offset, name in enumerate(checks):
                job_id = run_id * 10 + offset
                url = f"https://api.github.com/repos/{qualification.REPO}/check-runs/{job_id}"
                completed = {"name": name, "head_sha": self.source, "status": "completed", "conclusion": "success"}
                self.jobs[name] = {**completed, "id": job_id, "run_id": run_id, "run_attempt": 1, "check_run_url": url}
                self.checks[name] = {**completed, "app": {"id": 15368}, "check_suite": {"id": run_id}, "url": url}

    def api(self, route, *, binary=False):
        self.assertEqual(binary, route.endswith("/zip"))
        path = urlsplit(route).path
        query = parse_qs(urlsplit(route).query)
        if route in self.routes:
            return deepcopy(self.routes[route])
        for workflow, run in self.ci.items():
            if path == f"actions/workflows/{workflow}/runs":
                self.assertEqual(query, {"head_sha": [self.source], "branch": ["main"], "event": ["push"],
                                         "per_page": ["100"], "page": ["1"]})
                return {"total_count": 1, "workflow_runs": [deepcopy(run)]}
            if re.fullmatch(rf"actions/runs/{run['id']}/attempts/[0-9]+/jobs", path):
                return {"total_count": 2, "jobs": [deepcopy(self.jobs[name]) for name in qualification.CHECKS[workflow]]}
        if path == f"commits/{self.source}/check-runs":
            return {"check_runs": [deepcopy(self.checks[query["check_name"][0]])]}
        self.fail(f"unexpected API route {route}")

    def authorize(self):
        def git(*args):
            subprocess.run(["git", "-C", self.workdir, *args], check=True, timeout=10, capture_output=True)
        return qualification.authorize(self.env, self.inputs, self.fresh, self.lockfile, self.api, git)

    def test_unrelated_main_merges_preserve_the_candidate(self):
        proof = self.authorize()
        self.assertEqual((proof["source_sha"], proof["workflow_sha"], proof["checked_main_sha"]),
                         (self.source, self.workflow, self.tip))
        self.assertEqual(proof["plan"]["artifact_id"], 17)
        self.assertEqual(proof["recovery_pair"], self.inputs["recovery_pair"])
        self.assertEqual(proof["checks"]["check.yml"], {"run_id": 51, "attempt": 1})

    def test_refuses_changed_workflow_off_main_source_or_newer_staging(self):
        gates = ("wrangler/scripts/ownership-control.mjs", "wrangler/src/alert-rules.ts",
                 ".github/workflows/deploy-staging.yml")
        for number, path in enumerate(gates):
            self.git("switch", "-qc", f"gate-change-{number}", self.tip)
            self.env["FETCHED_MAIN_SHA"] = self.commit(path)
            self.workdir.mkdir(exist_ok=True)
            with self.subTest(changed_path=path), self.assertRaises(subprocess.CalledProcessError):
                self.authorize()
        self.env["FETCHED_MAIN_SHA"] = self.tip
        with self.assertRaises(subprocess.CalledProcessError):
            self.inputs["sha"] = self.env["SOURCE_SHA"] = self.off_main
            self.authorize()
        self.inputs["sha"] = self.env["SOURCE_SHA"] = self.source
        self.history["head_sha"] = self.workflow
        with self.assertRaises(subprocess.CalledProcessError):
            self.authorize()
        self.history["head_sha"] = self.prior
        self.history["run_number"] = 10
        with self.assertRaisesRegex(GateError, "reviewed_source_superseded"):
            self.authorize()

    def test_refuses_unprotected_reruns_and_mixed_modes(self):
        for key, value in (("GITHUB_REF_PROTECTED", "false"), ("GITHUB_RUN_ATTEMPT", "2"),
                           ("GITHUB_WORKFLOW_SHA", self.source)):
            with self.subTest(key=key):
                original = self.env[key]
                self.env[key] = value
                with self.assertRaisesRegex(GateError, "untrusted_reviewed_dispatch"):
                    self.authorize()
                self.env[key] = original
        for key in qualification.INCOMPATIBLE_INPUTS:
            self.inputs[key] = "true"
            with self.subTest(key=key), self.assertRaisesRegex(GateError, "clean_verified_recovery"):
                self.authorize()
            del self.inputs[key]

    def test_refuses_wrangler_incompatible_with_protected_receipt_gate(self):
        self.lockfile["packages"]["node_modules/wrangler"]["version"] = "4.0.0"
        with self.assertRaisesRegex(GateError, "reviewed_source_wrangler_incompatible"):
            self.authorize()

    def test_refuses_mismatched_plan_and_unbound_or_failed_latest_checks(self):
        fresh = self.fresh.copy()
        with self.assertRaisesRegex(GateError, "reviewed_plan_mismatch"):
            self.fresh["target_fingerprint"] = "0" * 64
            self.authorize()
        self.fresh = fresh
        self.checks["ci-ok"]["app"]["id"] = 99
        with self.assertRaisesRegex(GateError, "candidate_check_unproved"):
            self.authorize()
        self.checks["ci-ok"]["app"]["id"] = 15368
        self.jobs["ci-ok"]["run_attempt"] = 2
        with self.assertRaisesRegex(GateError, "candidate_check_job_unproved"):
            self.authorize()
        self.jobs["ci-ok"]["run_attempt"] = 1
        self.ci["check.yml"]["conclusion"] = "failure"
        with self.assertRaisesRegex(GateError, "candidate_ci_unproved"):
            self.authorize()

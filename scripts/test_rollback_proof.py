#!/usr/bin/env python3
"""Offline fixtures for the daily staging rollback proof; no live dispatches."""

from contextlib import redirect_stderr, redirect_stdout
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import rollback_proof as proof

NOW = datetime(2026, 10, 23, 6, 23, tzinfo=timezone.utc)
VERSION = "11111111-1111-1111-1111-111111111111"
DIGEST = "sha256:" + "a" * 64
ENV = {"GITHUB_REPOSITORY": proof.REPO, "GITHUB_REF": "refs/heads/main"}


def run(workflow=proof.WORKFLOW, event="workflow_dispatch", conclusion="failure"):
    return {"id": 123, "path": workflow, "event": event, "head_branch": "main",
            "repository": {"full_name": proof.REPO}, "head_repository": {"full_name": proof.REPO},
            "head_sha": "a" * 40, "run_attempt": 1, "status": "completed", "conclusion": conclusion,
            "created_at": NOW.isoformat(), "updated_at": NOW.isoformat()}


def files():
    return {"staging-rollback-snapshot.json": {
                "worker": "two-bot-next-staging", "has_previous": True,
                "serving_version": VERSION, "serving_image": "registry.example/image@" + DIGEST},
            "auto-rollback-evidence.json": {"worker": "two-bot-next-staging", "variant": "staging",
                "restored_version": VERSION, "restored_image_digest": DIGEST,
                "gate": "stable-200-gateway-ready", "takeover": True},
            "error-rate-gate.json": {"mode": "rehearsal", "verdict": "fail", "firing": ["rest_5xx_rate"]}}


def jobs():
    return {"total_count": 1, "jobs": [{"name": "wrangler deploy + intended rollout gate",
            "steps": [{"name": name, "conclusion": outcome} for name, outcome in proof.REQUIRED_STEPS.items()]}]}


class RollbackProofTests(unittest.TestCase):
    def test_dispatch_is_exactly_once_staging_main_and_restores_both_members(self):
        with patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, run(), jobs()]) as gh, \
                patch.object(proof, "artifact_files", return_value=files()) as artifacts, redirect_stdout(io.StringIO()):
            result = proof.prove()
        self.assertEqual(gh.call_count, 3)
        self.assertEqual(gh.call_args_list[0].args, (f"repos/{proof.REPO}/actions/workflows/deploy-staging.yml/dispatches",))
        self.assertEqual(gh.call_args_list[0].kwargs["body"],
                         {"ref": "main", "inputs": {"inject_error_rate_spike": True}})
        artifacts.assert_called_once_with(123, "staging-rollout-evidence")
        self.assertEqual(result["restored_version"], VERSION)
        self.assertEqual(result["restored_image_digest"], DIGEST)
        self.assertEqual(result["child_conclusion"], "failure")
        self.assertEqual(result["verdict"], "pass")

    def test_missing_dispatch_id_never_searches_latest_or_redispatches(self):
        for response in ({}, {"workflow_run_id": True}, {"workflow_run_id": "123"}):
            with self.subTest(response=response), patch.object(proof, "gh", return_value=response) as gh:
                with self.assertRaisesRegex(proof.ProofError, "dispatch_missing_exact_run_id"):
                    proof.prove()
                self.assertEqual(gh.call_count, 1)

    def test_child_provenance_or_unexpected_outcome_is_not_a_proof(self):
        mutations = [lambda r: r.update(conclusion="success"), lambda r: r.update(conclusion="cancelled"),
                     lambda r: r.update(head_branch="feature"), lambda r: r.update(path="deploy-production.yml"),
                     lambda r: r.update(head_repository={"full_name": "fork/repo"}),
                     lambda r: r.update(run_attempt=2), lambda r: r.update(id=124)]
        for mutate in mutations:
            child = run()
            mutate(child)
            with self.subTest(child=child), patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, child]), \
                    patch.object(proof, "artifact_files") as artifacts, redirect_stdout(io.StringIO()):
                with self.assertRaises(proof.ProofError):
                    proof.prove()
                artifacts.assert_not_called()

    def test_timeout_does_not_cancel_or_redispatch_a_mutating_child(self):
        child = run()
        child["status"] = "in_progress"
        with patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, child]) as gh, \
                redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(proof.ProofError, "staging_run_timeout"):
                proof.prove(clock=iter([0, proof.MAX_WAIT + 1]).__next__)
        self.assertEqual(gh.call_count, 2)

    def test_every_expected_step_is_required(self):
        for name in proof.REQUIRED_STEPS:
            job = jobs()
            next(s for s in job["jobs"][0]["steps"] if s["name"] == name)["conclusion"] = "skipped"
            with self.subTest(name=name), patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, run(), job]), \
                    patch.object(proof, "artifact_files") as artifacts, redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(proof.ProofError, "unexpected_staging_step_outcome"):
                    proof.prove()
                artifacts.assert_not_called()

    def test_evidence_rejects_wrong_version_image_staging_gate_or_injection(self):
        for file, key, value in [
            ("staging-rollback-snapshot.json", "has_previous", False),
            ("staging-rollback-snapshot.json", "worker", "two-bot-next-production"),
            ("staging-rollback-snapshot.json", "serving_image", "unversioned:latest"),
            ("auto-rollback-evidence.json", "restored_version", "22222222-2222-2222-2222-222222222222"),
            ("auto-rollback-evidence.json", "restored_image_digest", "sha256:" + "b" * 64),
            ("auto-rollback-evidence.json", "variant", "production"),
            ("auto-rollback-evidence.json", "takeover", False),
            ("auto-rollback-evidence.json", "gate", "ready-once"),
            ("auto-rollback-evidence.json", "failure", "image_rollout_timeout"),
            ("error-rate-gate.json", "mode", "live"),
            ("error-rate-gate.json", "verdict", "skip"),
            ("error-rate-gate.json", "firing", []),
        ]:
            evidence = files()
            evidence[file][key] = value
            with self.subTest(file=file, key=key), self.assertRaises(proof.ProofError):
                proof.validate_evidence(evidence)
        self.assertEqual(proof.validate_evidence(files())["gate"], "stable-200-gateway-ready")

    def test_artifact_is_exact_bounded_and_never_extracted(self):
        data = io.BytesIO()
        with zipfile.ZipFile(data, "w") as archive:
            for name, value in files().items():
                archive.writestr("../" + name, json.dumps(value))
        listing = {"total_count": 1, "artifacts": [{"id": 1, "name": "staging-rollout-evidence",
                    "size_in_bytes": len(data.getvalue()), "expired": False}]}
        with patch.object(proof, "gh", side_effect=[listing, data.getvalue()]):
            self.assertEqual(proof.artifact_files(123, "staging-rollout-evidence"), files())
        for change in (lambda d: d.update(total_count=101),
                       lambda d: d["artifacts"][0].update(expired=True),
                       lambda d: d["artifacts"][0].update(size_in_bytes=proof.MAX_BYTES + 1),
                       lambda d: d["artifacts"].append(deepcopy(d["artifacts"][0]))):
            bad = deepcopy(listing)
            change(bad)
            with self.subTest(bad=bad), patch.object(proof, "gh", return_value=bad) as gh:
                with self.assertRaises(proof.ProofError):
                    proof.artifact_files(123, "staging-rollout-evidence")
                self.assertEqual(gh.call_count, 1)

    def test_duplicate_json_keys_and_duplicate_basenames_are_refused(self):
        for entries in [ [("proof.json", '{"verdict":"fail","verdict":"pass"}')],
                         [("a/proof.json", "{}"), ("b/proof.json", "{}")]]:
            data = io.BytesIO()
            with zipfile.ZipFile(data, "w") as archive:
                for name, value in entries:
                    archive.writestr(name, value)
            listing = {"total_count": 1, "artifacts": [{"id": 1, "name": proof.ARTIFACT,
                       "size_in_bytes": len(data.getvalue()), "expired": False}]}
            with patch.object(proof, "gh", side_effect=[listing, data.getvalue()]), self.assertRaises(proof.ProofError):
                proof.artifact_files(123, proof.ARTIFACT)

    def test_freshness_requires_latest_schedule_green_and_actual_artifact(self):
        good = run(proof.PROOF_WORKFLOW, "schedule", "success")
        evidence = {"schema_version": 1, "verdict": "pass", "environment": "staging",
                    "child_conclusion": "failure", "child_run_id": 456}
        with patch.object(proof, "gh", return_value={"workflow_runs": [good]}), \
                patch.object(proof, "artifact_files", return_value={"proof.json": evidence}):
            self.assertEqual(proof.check_freshness(now=NOW)["proof_run_id"], 123)
        for mutate in (lambda r: r.update(conclusion="failure"), lambda r: r.update(conclusion="cancelled"),
                       lambda r: r.update(status="in_progress"), lambda r: r.update(event="workflow_dispatch"),
                       lambda r: r.update(created_at=(NOW - timedelta(hours=31)).isoformat()),
                       lambda r: r.update(created_at=(NOW + timedelta(hours=1)).isoformat())):
            bad = deepcopy(good)
            mutate(bad)
            with patch.object(proof, "gh", return_value={"workflow_runs": [bad]}), \
                    patch.object(proof, "artifact_files") as artifacts, self.assertRaises(proof.ProofError):
                proof.check_freshness(now=NOW)
            artifacts.assert_not_called()
        with patch.object(proof, "gh", return_value={"workflow_runs": []}), self.assertRaisesRegex(proof.ProofError, "missing"):
            proof.check_freshness(now=NOW)

    def test_watch_posts_only_on_a_finding_and_delivery_failure_is_red(self):
        for error in (None, proof.ProofError("daily_proof_missing"), ValueError("private-body")):
            with patch.dict(os.environ, ENV), patch.object(proof, "check_freshness", side_effect=error,
                    return_value={"verdict": "pass"}), patch.object(proof, "alert") as alert, \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                code = proof.main(["watch"])
            self.assertEqual(code, 0 if error is None else 1)
            self.assertEqual(alert.call_count, 0 if error is None else 1)
        with patch.dict(os.environ, ENV), patch.object(proof, "alert", side_effect=proof.ProofError("alert_delivery_failed")), \
                redirect_stderr(io.StringIO()):
            self.assertEqual(proof.main(["alert"]), 1)

    def test_dry_run_has_no_effects_and_missing_evidence_refuses_before_dispatch(self):
        with patch.object(proof, "gh") as gh, patch.object(proof, "alert") as alert, redirect_stdout(io.StringIO()):
            self.assertEqual(proof.main(["run", "--dry-run"]), 0)
            gh.assert_not_called()
            alert.assert_not_called()
        with patch.dict(os.environ, ENV), patch.object(proof, "prove") as prove, redirect_stderr(io.StringIO()):
            self.assertEqual(proof.main(["run"]), 1)
            prove.assert_not_called()

    def test_api_version_is_pinned_and_failures_never_echo_stderr_or_retry(self):
        output = subprocess.CompletedProcess([], 1, b"", b"private-body-secret")
        with patch.object(proof.subprocess, "run", return_value=output) as call:
            with self.assertRaisesRegex(proof.ProofError, "github_request_failed_no_retry"):
                proof.gh("repos/example")
        self.assertEqual(call.call_count, 1)
        self.assertIn("X-GitHub-Api-Version: 2026-03-10", call.call_args.args[0])

    def test_success_writes_only_sanitized_evidence(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, ENV), \
                patch.object(proof, "prove", return_value={"verdict": "pass", "child_run_id": 123}), \
                redirect_stdout(io.StringIO()):
            path = Path(directory) / "proof.json"
            self.assertEqual(proof.main(["run", "--evidence", str(path)]), 0)
            self.assertEqual(json.loads(path.read_text()), {"verdict": "pass", "child_run_id": 123})


if __name__ == "__main__":
    unittest.main()

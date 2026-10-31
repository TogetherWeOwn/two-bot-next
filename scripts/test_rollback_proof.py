#!/usr/bin/env python3
"""Offline fixtures for the daily staging rollback proof; no live dispatches."""

from contextlib import contextmanager, redirect_stderr, redirect_stdout
import importlib.util
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch
import zipfile

import incident_issue
import rollback_proof as proof

# Independent oracle: never derive fixtures or negative cases from the code
# under test. Deleting any guard entry must fail this contract and its cases.
EXPECTED_STEPS = {
    "Snapshot the pre-deploy rollback state": "success",
    "Deploy to staging": "success",
    "Verify exact new rollout and serving Worker/container readiness": "success",
    "Gate promotion on REST 5xx error rate": "failure",
    "Auto-roll back Worker version and container image": "success",
    "Upload the rollout evidence": "success",
}


@contextmanager
def local_gh(script):
    """Replace only gh's executable, retaining real pipes, timeouts and cleanup."""
    popen = subprocess.Popen
    children = []

    def launch(args, **kwargs):
        child = popen([sys.executable, "-c", script], **kwargs)
        children.append(child)
        return child

    with patch.object(proof.subprocess, "Popen", side_effect=launch) as call:
        yield call, children


NOW = datetime(2026, 10, 23, 6, 23, tzinfo=timezone.utc)
VERSION = "11111111-1111-1111-1111-111111111111"
DIGEST = "sha256:" + "a" * 64
ENV = {"GITHUB_REPOSITORY": proof.REPO, "GITHUB_REF": "refs/heads/main"}


def run(workflow=proof.WORKFLOW, event="workflow_dispatch", conclusion="failure"):
    return {"id": 123, "display_title": "staging rollback proof", "path": workflow, "event": event, "head_branch": "main",
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
            "steps": [{"name": name, "conclusion": outcome} for name, outcome in EXPECTED_STEPS.items()]}]}


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
                     lambda r: r.update(run_attempt=2), lambda r: r.update(id=124),
                     lambda r: r.update(display_title="deploy-staging")]
        for mutate in mutations:
            child = run()
            mutate(child)
            with self.subTest(child=child), patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, child]), \
                    patch.object(proof, "artifact_files") as artifacts, redirect_stdout(io.StringIO()):
                with self.assertRaises(proof.ProofError):
                    proof.prove()
                artifacts.assert_not_called()

    def test_transient_display_title_before_completion_is_tolerated(self):
        pending = run()
        pending.update(status="in_progress", conclusion=None, display_title="deploy-staging")
        with patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, pending, run(), jobs()]) as gh, \
                patch.object(proof, "artifact_files", return_value=files()) as artifacts, \
                redirect_stdout(io.StringIO()):
            result = proof.prove(sleep=lambda _: None, clock=lambda: 0)
        self.assertEqual(gh.call_count, 4)
        artifacts.assert_called_once_with(123, "staging-rollout-evidence")
        self.assertEqual(result["verdict"], "pass")
        self.assertEqual(result["child_run_id"], 123)

    def test_wrong_display_title_at_completion_still_fails_closed(self):
        child = run()
        child.update(display_title="deploy-staging")
        with patch.object(proof, "gh", side_effect=[{"workflow_run_id": 123}, child]), \
                patch.object(proof, "artifact_files") as artifacts, redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(proof.ProofError, "child_not_rehearsal"):
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
        self.assertEqual(proof.REQUIRED_STEPS, EXPECTED_STEPS)
        for name in EXPECTED_STEPS:
            for outcome in ("missing", "failure", "skipped", "success"):
                if outcome == EXPECTED_STEPS[name]:
                    continue
                job = jobs()
                steps = job["jobs"][0]["steps"]
                if outcome == "missing":
                    steps[:] = [step for step in steps if step["name"] != name]
                else:
                    next(s for s in steps if s["name"] == name)["conclusion"] = outcome
                with self.subTest(name=name, outcome=outcome), patch.object(proof, "gh",
                        side_effect=[{"workflow_run_id": 123}, run(), job]), \
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

    def test_freshness_receipt_failures_execute_the_watcher_and_alert(self):
        proof_check = proof.check_freshness
        good = run(proof.PROOF_WORKFLOW, "schedule", "success")
        receipt = {"schema_version": 1, "verdict": "pass", "environment": "staging",
                   "child_conclusion": "failure", "child_run_id": 456}
        invalid = []
        for key, values in {
            "schema_version": (None, True, 2, "1"), "verdict": (None, "fail"),
            "environment": (None, "production"), "child_conclusion": (None, "success"),
            "child_run_id": (None, "456", True, 0, -1, 1.5),
        }.items():
            for value in values:
                invalid.append(dict(receipt, **{key: value}))
            missing = dict(receipt)
            del missing[key]
            invalid.append(missing)
        for evidence in invalid + [None, []]:
            with self.subTest(evidence=evidence), patch.dict(os.environ, ENV), \
                    patch.object(proof, "gh", return_value={"workflow_runs": [good]}), \
                    patch.object(proof, "artifact_files", return_value={"proof.json": evidence}) as artifacts, \
                    patch.object(proof, "check_freshness", wraps=lambda: proof_check(now=NOW)), \
                    patch.object(incident_issue, "open_incident") as opened, \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()) as stderr:
                self.assertEqual(proof.main(["watch"]), 1)
                self.assertEqual(json.loads(stderr.getvalue())["code"], "daily_proof_evidence_invalid")
                artifacts.assert_called_once_with(123, "staging-rollback-proof")
                self.assertEqual(opened.call_count, 1)
                self.assertIn("daily_proof_evidence_invalid", opened.call_args.args[3])

    def test_freshness_exact_age_boundaries(self):
        for delta, passes in ((timedelta(0), True), (timedelta(hours=30), True),
                              (timedelta(hours=30, microseconds=1), False),
                              (timedelta(microseconds=-1), False)):
            child = run(proof.PROOF_WORKFLOW, "schedule", "success")
            child["created_at"] = (NOW - delta).isoformat()
            evidence = {"schema_version": 1, "verdict": "pass", "environment": "staging",
                        "child_conclusion": "failure", "child_run_id": 456}
            with self.subTest(delta=delta), patch.object(proof, "gh", return_value={"workflow_runs": [child]}), \
                    patch.object(proof, "artifact_files", return_value={"proof.json": evidence}):
                if passes:
                    self.assertEqual(proof.check_freshness(now=NOW)["verdict"], "pass")
                else:
                    with self.assertRaisesRegex(proof.ProofError, "daily_proof_stale"):
                        proof.check_freshness(now=NOW)

    def test_a_newer_failed_manual_proof_keeps_the_incident_open(self):
        scheduled = run(proof.PROOF_WORKFLOW, "schedule", "success")
        scheduled["created_at"] = (NOW - timedelta(hours=2)).isoformat()
        evidence = {"schema_version": 1, "verdict": "pass", "environment": "staging",
                    "child_conclusion": "failure", "child_run_id": 456}
        for conclusion, newer, passes in (("failure", True, False), ("failure", False, True),
                                          ("success", True, True), ("cancelled", True, True),
                                          (None, True, True)):
            manual = dict(run(proof.PROOF_WORKFLOW, "workflow_dispatch", conclusion), id=124)
            manual["created_at"] = (NOW - timedelta(hours=1 if newer else 3)).isoformat()

            def fake_gh(path, scheduled=scheduled, manual=manual):
                return {"workflow_runs": [scheduled if "event=schedule" in path else manual]}

            with self.subTest(conclusion=conclusion, newer=newer), patch.object(proof, "gh", side_effect=fake_gh), \
                    patch.object(proof, "artifact_files", return_value={"proof.json": evidence}):
                if passes:
                    self.assertEqual(proof.check_freshness(now=NOW)["verdict"], "pass")
                else:
                    with self.assertRaisesRegex(proof.ProofError, "newer_proof_failed"):
                        proof.check_freshness(now=NOW)

    def test_watch_files_only_on_a_finding_closes_on_pass_and_delivery_failure_is_red(self):
        for error in (None, proof.ProofError("daily_proof_missing"), ValueError("private-body")):
            with patch.dict(os.environ, ENV), \
                    patch.object(proof, "check_freshness", side_effect=error, return_value={"verdict": "pass"}), \
                    patch.object(incident_issue, "open_incident") as opened, \
                    patch.object(incident_issue, "close_incident") as closed, \
                    patch.object(incident_issue, "run_link", return_value="https://example.test/run/123"), \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()) as stderr:
                code = proof.main(["watch"])
            self.assertEqual(code, 0 if error is None else 1)
            if error is None:
                opened.assert_not_called()
                closed.assert_called_once()
                self.assertEqual(closed.call_args.args[1], "staging-rollback-proof")
            else:
                finding = "daily_proof_missing" if isinstance(error, proof.ProofError) else "daily_proof_unreadable"
                opened.assert_called_once_with(os.environ, "staging-rollback-proof",
                                               "staging rollback proof failed", self.alert_text(finding))
                closed.assert_not_called()
            self.assertNotIn("private-body", stderr.getvalue())
        with patch.dict(os.environ, ENV), \
                patch.object(incident_issue, "open_incident",
                             side_effect=incident_issue.IncidentError("private-body")) as opened, \
                redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(proof.main(["alert"]), 1)
        self.assertEqual(opened.call_count, 1)
        self.assertEqual(json.loads(stderr.getvalue())["code"], "alert_delivery_failed")
        self.assertNotIn("private-body", stderr.getvalue())
        # A passing watch that cannot close the issue is red, never silent.
        with patch.dict(os.environ, ENV), \
                patch.object(proof, "check_freshness", return_value={"verdict": "pass"}), \
                patch.object(incident_issue, "close_incident",
                             side_effect=incident_issue.IncidentError("private-body")), \
                redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(proof.main(["watch"]), 1)
        self.assertEqual(json.loads(stderr.getvalue())["code"], "incident_close_failed")

    @staticmethod
    def alert_text(code):
        return (f"two-bot-next ALERT staging rollback proof: {code}. "
                "Inspect the daily staging drill and its child run before retrying; never drill production. "
                "Runbook: https://github.com/TogetherWeOwn/two-bot-next/blob/main/docs/production-deploy.md#daily-staging-proof "
                "| logs https://example.test/run/123")

    def test_alert_command_files_exactly_one_incident(self):
        with patch.dict(os.environ, ENV), \
                patch.object(incident_issue, "open_incident") as opened, \
                patch.object(incident_issue, "run_link", return_value="https://example.test/run/123"):
            self.assertEqual(proof.main(["alert"]), 0)
        opened.assert_called_once_with(os.environ, "staging-rollback-proof",
                                       "staging rollback proof failed", self.alert_text("daily_proof_failed"))

    def test_dry_run_has_no_effects_and_missing_evidence_refuses_before_dispatch(self):
        with patch.object(proof, "gh") as gh, patch.object(proof, "alert") as alert, redirect_stdout(io.StringIO()):
            self.assertEqual(proof.main(["run", "--dry-run"]), 0)
            gh.assert_not_called()
            alert.assert_not_called()
        with patch.dict(os.environ, ENV), patch.object(proof, "prove") as prove, redirect_stderr(io.StringIO()):
            self.assertEqual(proof.main(["run"]), 1)
            prove.assert_not_called()

    def test_api_version_is_pinned_and_failures_never_echo_stderr_or_retry(self):
        with local_gh("import sys; sys.stderr.write('private-body-secret'); sys.exit(1)") as (call, children):
            with self.assertRaisesRegex(proof.ProofError, "^github_request_failed_no_retry$"):
                proof.gh("repos/example")
        self.assertEqual(call.call_count, 1)
        self.assertEqual(call.call_args.args[0], ["gh", "api", "--method", "GET", "-H",
                         "X-GitHub-Api-Version: 2026-03-10", "repos/example"])
        self.assertIsNotNone(children[0].returncode)

    def test_body_bearing_dispatch_uses_exact_post_argv_and_input_bytes(self):
        path = "repos/TogetherWeOwn/two-bot-next/actions/workflows/deploy-staging.yml/dispatches"
        body = {"ref": "main", "inputs": {"inject_error_rate_spike": True}}
        script = ("import json,sys; data=sys.stdin.buffer.read(); "
                  "print(json.dumps({'workflow_run_id':123,'input':data.decode()}))")
        with local_gh(script) as (call, children):
            response = proof.gh(path, body=body)
        self.assertEqual(response, {"workflow_run_id": 123, "input": json.dumps(body)})
        call.assert_called_once_with(["gh", "api", "--method", "POST", "-H",
            "X-GitHub-Api-Version: 2026-03-10", path, "--input", "-"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertEqual(proof.GH_TIMEOUT, 45)
        self.assertEqual(children[0].returncode, 0)

    def test_empty_invalid_or_duplicate_json_response_fails_closed(self):
        for raw in (b"", b"not-json", b'{"workflow_run_id":123,"workflow_run_id":124}'):
            with self.subTest(raw=raw), local_gh(f"import sys; sys.stdout.buffer.write({raw!r})") as (call, _):
                with self.assertRaises(proof.ProofError):
                    proof.gh("repos/example")
                self.assertEqual(call.call_count, 1)

    def test_streaming_limits_both_pipes_before_child_finishes_and_reaps_it(self):
        # The child writes cap+1 then sleeps. A post-buffer check cannot finish
        # before the timeout; it must be killed for overflow, not timeout.
        for stream in (1, 2):
            script = f"import os,time; os.write({stream}, b'x' * {proof.MAX_BYTES + 1}); time.sleep(60)"
            with self.subTest(stream=stream), patch.object(proof, "GH_TIMEOUT", 3), \
                    local_gh(script) as (call, children):
                start = time.monotonic()
                with self.assertRaisesRegex(proof.ProofError, "^github_response_too_large$"):
                    proof.gh("repos/example", binary=True)
                self.assertLess(time.monotonic() - start, 3)
                self.assertEqual(call.call_count, 1)
                self.assertIsNotNone(children[0].returncode)
        # Exactly at the cap is allowed, including stderr drained before stdout.
        script = f"import os; os.write(2, b'x' * {proof.MAX_BYTES}); os.write(1, b'y' * {proof.MAX_BYTES})"
        with local_gh(script):
            self.assertEqual(proof.gh("repos/example", binary=True), b"y" * proof.MAX_BYTES)

    def test_subprocess_timeout_kills_and_reaps_without_retry(self):
        with patch.object(proof, "GH_TIMEOUT", 0.2), local_gh("import time; time.sleep(60)") as (call, children):
            with self.assertRaisesRegex(proof.ProofError, "^github_transport_failure$"):
                proof.gh("repos/example")
            self.assertEqual(call.call_count, 1)
            self.assertIsNotNone(children[0].returncode)

    def test_subprocess_timeout_after_both_pipes_close_kills_and_reaps_without_retry(self):
        # Valid output and EOF do not mean the process has exited. Keep the
        # mutant finite so removing the final wait timeout fails, not hangs.
        script = ("import os,time; os.write(1, b'{\"workflow_run_id\":123}'); "
                  "os.close(1); os.close(2); time.sleep(2)")
        wait = subprocess.Popen.wait
        with patch.object(proof, "GH_TIMEOUT", 0.5), \
                patch.object(subprocess.Popen, "wait", autospec=True, side_effect=wait) as waits, \
                local_gh(script) as (call, children):
            with self.assertRaisesRegex(proof.ProofError, "^github_transport_failure$"):
                proof.gh("repos/example")
            self.assertEqual(call.call_count, 1)
            self.assertIsNotNone(children[0].returncode)
            self.assertNotEqual(children[0].returncode, 0)
            # Prove EOF reached the bounded exit wait, not the selector timeout.
            self.assertTrue(any(item.kwargs.get("timeout", 0) > 0 for item in waits.call_args_list))

    def test_success_writes_only_sanitized_evidence(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, ENV), \
                patch.object(proof, "prove", return_value={"verdict": "pass", "child_run_id": 123}), \
                redirect_stdout(io.StringIO()):
            path = Path(directory) / "proof.json"
            self.assertEqual(proof.main(["run", "--evidence", str(path)]), 0)
            self.assertEqual(json.loads(path.read_text()), {"verdict": "pass", "child_run_id": 123})


class PromotionAndHoldTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Reuse the existing offline guard harness without modifying its
        # Test Author acceptance cases (including short-SHA protection).
        spec = importlib.util.spec_from_file_location("production_guard_fixture",
            Path(__file__).with_name("test-deploy-production.py"))
        cls.harness = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.harness)

    def promotion(self, mutate_run=None, mutate_jobs=None, mutate_current=None, event="workflow_dispatch", live=False):
        h = self.harness
        child = dict(run(), id=502, run_number=6, head_sha=h.SHA)
        if mutate_run:
            mutate_run(child)
        child_jobs = jobs()
        child_jobs["jobs"][0].update(run_id=502, run_attempt=1, head_sha=h.SHA,
                                      status="completed", conclusion="failure")
        if mutate_jobs:
            mutate_jobs(child_jobs)
        history = [h.staging_run(), dict(child)]
        current = deepcopy(child)
        if mutate_current:
            mutate_current(current)
        # The guard re-reads the exact run and attempt, not just the listing.
        return h.GuardBehaviourTests().guard(auto="true", event=event, **{
            "staging-all": {"total_count": 2, "workflow_runs": history},
            "ci-run": lambda path, query: current if path.endswith("/502") else h.workflow_run(),
            "ci-jobs": lambda path, query: child_jobs if "/502/" in path else h.green()["ci-jobs"],
            "production-readyz": {"status": 200, "build_revision": h.SHA if live else "f" * 40},
        })

    def test_verified_expected_red_rehearsal_does_not_hide_green_certification(self):
        for event, live in (("workflow_dispatch", False), ("schedule", False), ("schedule", True)):
            with self.subTest(event=event, live=live):
                result = self.promotion(event=event, live=live)
                self.assertEqual(result[0], 0, result[4])
                self.assertEqual(result[1]["sha"], self.harness.SHA)
                self.assertEqual(result[1]["skip"], "live" if live else "")
                self.assertTrue(any(path.endswith("/actions/runs/502") for path, _ in result[3]))
                self.assertTrue(any("/502/attempts/1/jobs" in path for path, _ in result[3]))

    def test_listing_cannot_substitute_for_the_current_exact_run(self):
        for key, value in (("id", 503), ("run_number", 7), ("head_sha", "b" * 40), ("run_attempt", 2)):
            with self.subTest(key=key):
                result = self.promotion(mutate_current=lambda row: row.update({key: value}))
                self.assertEqual(result[0], 1, result[4])
                self.assertEqual(result[1], {})
                self.assertTrue(any(path.endswith("/actions/runs/502") for path, _ in result[3]))
                self.assertFalse(any("/502/attempts/1/jobs" in path for path, _ in result[3]))

    def test_name_alone_or_wrong_provenance_never_excuses_red_history(self):
        for key, value in (("display_title", "ordinary deploy"), ("event", "push"),
                           ("path", ".github/workflows/other.yml"), ("head_branch", "feature"),
                           ("head_repository", {"full_name": "fork/repo"}), ("run_attempt", 2),
                           ("status", "in_progress"), ("conclusion", "cancelled")):
            with self.subTest(key=key):
                result = self.promotion(mutate_run=lambda row: row.update({key: value}))
                self.assertEqual(result[0], 1, result[4])
                self.assertEqual(result[1], {})

    def test_wrong_or_missing_proof_job_identity_and_outcome_never_excuses_red_history(self):
        # Mutate only job metadata: the listing, re-read run and all six steps
        # still match, so none of their guards can mask a lost job binding.
        for key, values in {
            "run_id": (503, None), "run_attempt": (2, None),
            "head_sha": ("b" * 40, None), "status": ("in_progress", "queued", None),
            "conclusion": ("success", "cancelled", "skipped", None),
        }.items():
            for value in (*values, "missing"):
                def mutate(data):
                    job = data["jobs"][0]
                    if value == "missing":
                        del job[key]
                    else:
                        job[key] = value

                with self.subTest(key=key, value=value):
                    result = self.promotion(mutate_jobs=mutate)
                    self.assertEqual(result[0], 1, result[4])
                    self.assertEqual(result[1], {})
                    self.assertTrue(any("/502/attempts/1/jobs" in path for path, _ in result[3]))

    def test_missing_failed_or_skipped_restore_chain_never_excuses_red_history(self):
        for name, required in EXPECTED_STEPS.items():
            for outcome in ("missing", "failure", "skipped", "success"):
                if outcome == required:
                    continue

                def mutate(data):
                    steps = data["jobs"][0]["steps"]
                    if outcome == "missing":
                        steps[:] = [step for step in steps if step["name"] != name]
                    else:
                        next(step for step in steps if step["name"] == name)["conclusion"] = outcome

                with self.subTest(name=name, outcome=outcome):
                    result = self.promotion(mutate_jobs=mutate)
                    self.assertEqual(result[0], 1, result[4])
                    self.assertEqual(result[1], {})

    def test_daily_rehearsal_honors_the_hold_but_manual_arm_disarm_keeps_exception(self):
        lines = (Path(__file__).resolve().parents[1] / ".github/workflows/deploy-staging.yml").read_text().splitlines()
        start = lines.index("      - name: Defer the push while the soak hold is armed")
        start = lines.index("        run: |", start) + 1
        end = start
        while end < len(lines) and (not lines[end].strip() or lines[end].startswith("          ")):
            end += 1
        shell = textwrap.dedent("\n".join(lines[start:end]))
        now = datetime.now(timezone.utc)
        for rehearsal, hold, expected in (
            ("true", now + timedelta(hours=2), 1), ("false", now + timedelta(hours=2), 0),
            ("true", now - timedelta(seconds=60), 0), ("true", "invalid", 1),
            ("true", now + timedelta(hours=5), 1), ("true", "", 0),
        ):
            raw = hold.isoformat() if isinstance(hold, datetime) else hold
            with self.subTest(rehearsal=rehearsal, hold=raw), \
                    tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as directory:
                env = dict(os.environ, EVENT="workflow_dispatch", REHEARSAL=rehearsal,
                           HOLD_UNTIL=raw, GITHUB_STEP_SUMMARY=str(Path(directory) / "summary"))
                result = subprocess.run(["bash", "-c", shell], env=env, capture_output=True, timeout=5)
                self.assertEqual(result.returncode, expected, result.stdout)


if __name__ == "__main__":
    unittest.main()

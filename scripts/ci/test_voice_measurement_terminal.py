"""Contract tests for terminal job-graph classification."""

import json
import os
import subprocess
import sys
import tempfile
import unittest

from pathlib import Path
from unittest.mock import patch

from voice_measurement_terminal import (
    JOBS, candidate_verified, evaluate_terminal, lifecycles_verified,
    terminal_verdict, main,
)


class VoiceMeasurementTerminalTests(unittest.TestCase):
    def test_cli_writes_exclusive_fixed_verdict_and_refuses_missing_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / "terminal.json"
            env = {"EVIDENCE_DIR": directory, "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1",
                   "STAGING_USE_EXPIRES_AT": "123456", "STAGING_USE_SHA": "a" * 40,
                   "STAGING_USE_DEPLOYMENT": "deploy", "SECOND_REQUIRED": "false"}
            with patch("sys.argv", ["terminal", str(out)]), patch.dict("os.environ", env, clear=True), \
                 patch("voice_measurement_terminal.evaluate_terminal", return_value="accepted"):
                self.assertEqual(main(), 0)
                self.assertEqual(json.loads(out.read_text()),
                                 {"schema": "voice-measurement-terminal.v1", "verdict": "accepted"})
                self.assertEqual(main(), 1)
            out.unlink()
            with patch("sys.argv", ["terminal", str(out)]), patch.dict("os.environ", {}, clear=True):
                self.assertEqual(main(), 1)
                self.assertEqual(json.loads(out.read_text())["verdict"], "refused")
            link = Path(directory) / "link"
            link.symlink_to(out)
            with patch("sys.argv", ["terminal", str(link)]), patch.dict("os.environ", {}, clear=True):
                self.assertEqual(main(), 1)

    def test_real_candidate_artifact_layout_and_missing_evidence(self):
        # Generate real receipts and decisions with the production comparator.
        script = """
import { buildReceipt } from './wrangler/scripts/voice-rest-receipt.mjs';
import { firstIntervalDecision, combinedDecision } from './wrangler/scripts/voice-measurement-decision.mjs';
const capturedAt = Date.now();
const expires = capturedAt + 3600000;
const metadata = { runId: '123', runAttempt: '1', sha: 'a'.repeat(40),
  deploymentId: 'deploy', expiresAt: new Date(expires).toISOString() };
const receipt = (value, offset) => buildReceipt({ metadata, now: () => new Date(capturedAt + offset), samples: [
  { name: 'two_bot_rest_requests_total', labels: { result: '2xx' }, value },
  ...[1, 2, 3, 4].map((value, i) => ({ name: 'two_bot_process_incarnation',
    labels: { part: String(i) }, value })),
] });
const before = receipt(5, -2000), middle = receipt(9, -1000), after = receipt(20, 0);
console.log(JSON.stringify({ expires, metadata, before, middle, after,
  one: firstIntervalDecision(before, after), two: combinedDecision(before, middle, after) }));
"""
        fixture = json.loads(subprocess.check_output(["node", "--input-type=module", "-e", script], text=True))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("candidate/voice-before", "candidate/voice-middle", "cleanup", "first", "second"):
                (root / name).mkdir(parents=True)
            def write(name, value):
                (root / name).write_text(json.dumps(value), encoding="utf-8")

            write("grant.json", fixture["metadata"])
            write("candidate/voice-before/voice-before.json", fixture["before"])
            write("candidate/voice-middle/voice-after.json", fixture["middle"])
            write("candidate/voice-after.json", fixture["after"])
            write("candidate/voice-final.json", fixture["after"])
            write("candidate/voice-decision.json", fixture["one"])
            write("candidate/voice-combined.json", fixture["two"])
            checks = [{"check": name, "verdict": "PASS"} for name in ("create", "moved", "delete")]
            for name in ("first", "second"):
                write(f"{name}/voice-synthetic-123-1.json", {"checks": checks})
            correlation = {"environment": "staging", "runId": "123", "runAttempt": 1}
            use_start = fixture["expires"] - 3602000  # Exactly the first capture.
            use = dict(correlation, expiresAt=fixture["expires"], acquiredAt=use_start,
                       phase="released", sha="a" * 40, deploymentId="deploy", epoch=1)
            records = [dict(correlation, schema="two-bot-staging-pipeline.v1", action="attempt", operation="finish"),
                       dict(correlation, schema="two-bot-staging-use.v1", action="status",
                            sha="a" * 40, deploymentId="deploy", epoch=1, running=True,
                            use=dict(use, phase="active")),
                       dict(correlation, schema="two-bot-staging-use.v1", action="release",
                            running=False, sha="a" * 40, deploymentId="deploy", epoch=1, use=use)]
            cleanup = root / "cleanup/staging-use-measurement-stop.jsonl"

            def write_cleanup(value):
                cleanup.write_text("\n".join(map(json.dumps, value)) + "\n", encoding="utf-8")

            env = {"PATH": os.environ.get("PATH", ""), "EVIDENCE_DIR": directory,
                   "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1",
                   "STAGING_USE_EXPIRES_AT": str(fixture["expires"]), "STAGING_USE_SHA": "a" * 40,
                   "STAGING_USE_DEPLOYMENT": "deploy"}
            output = root / "terminal.json"

            def assert_verdict(verdict, command=main):
                self.assertEqual(command(), 0 if verdict == "accepted" else 1)
                self.assertEqual(json.loads(output.read_text())["verdict"], verdict)
                output.unlink()

            for second in ("false", "true"):
                env["SECOND_REQUIRED"] = second
                env.update({"RESULT_" + job.upper().replace("-", "_"): "success" for job in JOBS})
                if second == "false":
                    env.update(RESULT_VOICE_SECOND="skipped", RESULT_MEASUREMENT_FINAL="skipped")
                with patch("sys.argv", ["terminal", str(output)]), patch.dict("os.environ", env, clear=True):
                    for offset, verdict in ((0, "accepted"), (500, "refused")):
                        with self.subTest(second=second, acquisition_offset=offset):
                            for record in records[1:]:
                                record["use"]["acquiredAt"] = use_start + offset
                            write_cleanup(records)
                            assert_verdict(verdict)
                    for record in records[1:]:
                        record["use"]["acquiredAt"] = use_start
                    altered = json.loads(json.dumps(records))
                    for record in altered[1:]:
                        record["sha"] = record["use"]["sha"] = "b" * 40
                    write_cleanup(altered)
                    with patch.dict("os.environ", {"STAGING_USE_SHA": "b" * 40}):
                        assert_verdict("refused")
                        write("grant.json", dict(fixture["metadata"], sha="b" * 40))
                        assert_verdict("refused")
                    write("grant.json", fixture["metadata"])
                    for index in (1, 2):
                        with self.subTest(second=second, missing_correlation=index):
                            altered = [dict(record) for record in records]
                            del altered[index]["runId"]
                            write_cleanup(altered)
                            assert_verdict("refused")
                    write_cleanup(records)
                    paths = ["grant.json", "cleanup/staging-use-measurement-stop.jsonl",
                             "first/voice-synthetic-123-1.json", "candidate/voice-before/voice-before.json"]
                    paths += (["second/voice-synthetic-123-1.json", "candidate/voice-middle/voice-after.json",
                               "candidate/voice-final.json", "candidate/voice-combined.json"] if second == "true"
                              else ["candidate/voice-after.json", "candidate/voice-decision.json"])
                    for path in paths:
                        with self.subTest(second=second, missing=path):
                            missing = root / path
                            saved = missing.read_bytes()
                            missing.unlink()
                            assert_verdict("refused")
                            missing.write_bytes(saved)
                    grant = root / "grant.json"
                    grant.unlink()
                    os.mkfifo(grant)
                    assert_verdict("refused", lambda: subprocess.run(
                        [sys.executable, "scripts/ci/voice_measurement_terminal.py", output], timeout=5,
                    ).returncode)
                    grant.unlink()
                    write("grant.json", fixture["metadata"])

    def test_lifecycles_require_raw_success_for_the_selected_branch(self):
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "first"
            second = Path(directory) / "second"
            first.mkdir()
            second.mkdir()
            checks = [{"check": name, "verdict": "PASS"} for name in ("create", "moved", "delete")]
            for root in (first, second):
                (root / "voice-synthetic-123-1.json").write_text(json.dumps({"checks": checks}))
            self.assertTrue(lifecycles_verified(first, None, "123", "false"))
            self.assertTrue(lifecycles_verified(first, second, "123", "true"))
            self.assertFalse(lifecycles_verified(first, second, "123", "false"))
            self.assertFalse(lifecycles_verified(first, None, "123", "true"))
            self.assertFalse(lifecycles_verified(first, second, "124", "true"))
            self.assertFalse(lifecycles_verified(first, second, "123", "unknown"))
            checks[0]["verdict"] = "FAIL"
            (second / "voice-synthetic-123-1.json").write_text(json.dumps({"checks": checks}))
            self.assertFalse(lifecycles_verified(first, second, "123", "true"))

    def test_candidate_verifier_refuses_invalid_inputs_and_process_failures(self):
        args = ({"runId": "123"}, 100, "candidate", ["before", "after"])
        with patch("voice_measurement_terminal.subprocess.run") as run:
            run.return_value.returncode = 0
            self.assertTrue(candidate_verified("false", *args))
            self.assertEqual(run.call_args.args[0][-5:], ["false", "-", "candidate", "before", "after"])
            self.assertEqual(json.loads(run.call_args.kwargs["input"]), {"runId": "123", "acquiredAt": 100})
            for stream in ("stdout", "stderr"):
                self.assertEqual(run.call_args.kwargs[stream], subprocess.DEVNULL)
            self.assertEqual(run.call_args.kwargs["timeout"], 15)
            for branch in ("true", "unknown"):
                self.assertFalse(candidate_verified(branch, *args))
            run.return_value.returncode = 1
            for error in (None, subprocess.TimeoutExpired("node", 15), FileNotFoundError()):
                run.side_effect = error
                self.assertFalse(candidate_verified("false", *args))

    def test_terminal_requires_all_independent_evidence(self):
        results = dict.fromkeys(JOBS, "success")
        args = (results, "true", "123", 1, 123456, "a" * 40, "deploy",
                "cleanup", "first", "second", "grant", "candidate", ["before", "middle", "after"])
        records = [{}, {}, {"use": {"acquiredAt": 100}}]
        metadata = {"runId": "123"}
        with patch("voice_measurement_terminal.read_cleanup_receipt", return_value=records) as read, \
             patch("voice_measurement_terminal.cleanup_verified", return_value=True) as cleanup, \
             patch("voice_measurement_terminal.lifecycles_verified", return_value=True) as lifecycle, \
             patch("voice_measurement_terminal.read_matching_grant", return_value=metadata) as grant, \
             patch("voice_measurement_terminal.candidate_verified", return_value=True) as candidate:
            self.assertEqual(evaluate_terminal(*args), "accepted")
            read.assert_called_with("cleanup")
            cleanup.assert_called_with(records, "123", 1, 123456, "a" * 40, "deploy")
            lifecycle.assert_called_with("first", "second", "123", "true")
            candidate.assert_called_with("true", metadata, 100, "candidate", ["before", "middle", "after"])
            for check in (cleanup, lifecycle, grant, candidate):
                with patch.object(check, "return_value", None):
                    self.assertEqual(evaluate_terminal(*args), "refused")
            failed = dict(results, **{"staging-use-release": "failure"})
            read.reset_mock()
            self.assertEqual(evaluate_terminal(failed, *args[1:]), "refused")
            read.assert_not_called()

    def test_both_success_paths_require_verified_evidence(self):
        results = dict.fromkeys(JOBS, "success")
        self.assertEqual(terminal_verdict(results, "true", True, True), "accepted")
        results.update({"voice-second": "skipped", "measurement-final": "skipped"})
        self.assertEqual(terminal_verdict(results, "false", True, True), "accepted")
        for cleanup, candidate in ((False, True), (True, False), (None, True), (True, None)):
            self.assertEqual(terminal_verdict(results, "false", cleanup, candidate), "refused")

    def test_each_unexpected_job_outcome_refuses(self):
        for second in ("true", "false"):
            expected = dict.fromkeys(JOBS, "success")
            if second == "false":
                expected.update({"voice-second": "skipped", "measurement-final": "skipped"})
            for job in JOBS:
                for result in ("failure", "cancelled", "", "unknown"):
                    with self.subTest(second=second, job=job, result=result):
                        changed = dict(expected, **{job: result})
                        self.assertEqual(terminal_verdict(changed, second, True, True), "refused")
            for job in JOBS:
                changed = dict(expected)
                del changed[job]
                self.assertEqual(terminal_verdict(changed, second, True, True), "refused")
            self.assertEqual(terminal_verdict(dict(expected, unexpected="success"), second, True, True), "refused")
            self.assertEqual(terminal_verdict(expected, "", True, True), "refused")


if __name__ == "__main__":
    unittest.main()

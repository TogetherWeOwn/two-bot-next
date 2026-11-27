"""Contract tests for terminal job-graph classification."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import yaml

from voice_measurement_policy import WORKFLOW
from voice_measurement_terminal import JOBS, candidate_verified, main, terminal_verdict


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = """
import { buildReceipt } from './wrangler/scripts/voice-rest-receipt.mjs';
import { firstIntervalDecision, combinedDecision } from './wrangler/scripts/voice-measurement-decision.mjs';
const end = Date.now(), expires = end + 3600000;
const metadata = { runId: '123', runAttempt: '1', sha: 'a'.repeat(40),
  deploymentId: 'deploy', expiresAt: new Date(expires).toISOString() };
const receipt = (value, offset) => buildReceipt({ metadata, now: () => new Date(end + offset), samples: [
  { name: 'two_bot_rest_requests_total', labels: { result: '2xx' }, value },
  ...[1, 2, 3, 4].map((value, i) => ({ name: 'two_bot_process_incarnation',
    labels: { part: String(i) }, value })),
] });
const before = receipt(5, -2000), middle = receipt(9, -1000), after = receipt(20, 0);
console.log(JSON.stringify({ expires, metadata, before, middle, after,
  one: firstIntervalDecision(before, after), two: combinedDecision(before, middle, after),
  early: new Date(end - 1).toISOString() }));
"""


def outcomes(second):
    results = dict.fromkeys(JOBS, "success")
    if second == "false":
        results.update({"voice-second": "skipped", "measurement-final": "skipped"})
    return results


class VoiceMeasurementTerminalTests(unittest.TestCase):
    def test_sparse_checkout_runs_both_terminal_paths(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows" / WORKFLOW).read_text(encoding="utf-8"))
        options = workflow["jobs"]["measurement-terminal"]["steps"][0]["with"]
        self.assertIs(options["persist-credentials"], False)
        self.assertEqual(options["fetch-depth"], 1)
        self.assertIs(options.get("sparse-checkout-cone-mode"), False)
        patterns = options["sparse-checkout"]
        fixture = json.loads(subprocess.check_output(
            ["node", "--input-type=module", "-e", FIXTURE], cwd=ROOT, timeout=10,
        ))
        source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, timeout=10).strip()
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory) / "source"
            subprocess.run(["git", "clone", "--quiet", "--shared", "--no-checkout", ROOT, checkout],
                           check=True, timeout=20)
            subprocess.run(["git", "sparse-checkout", "set", "--no-cone", "--stdin"], cwd=checkout,
                           input=patterns, text=True, check=True, timeout=10)
            subprocess.run(["git", "checkout", "--quiet", "--detach", source], cwd=checkout,
                           check=True, timeout=20)
            paths = [pattern.removeprefix("/") for pattern in patterns.splitlines()]
            expected = subprocess.check_output(
                ["git", "ls-tree", "-r", "--name-only", "HEAD", "--", *paths], cwd=checkout, text=True, timeout=10,
            )
            entries = subprocess.check_output(["git", "ls-files", "-t"], cwd=checkout, text=True, timeout=10)
            self.assertEqual({entry[2:] for entry in entries.splitlines() if entry.startswith("H ")},
                             set(expected.splitlines()))
            for second in ("false", "true"):
                with self.subTest(second=second), tempfile.TemporaryDirectory(dir=directory) as evidence:
                    self.check_branch(Path(evidence), fixture, second, checkout)

    def check_branch(self, root, f, second, checkout):
        two = second == "true"
        before = "candidate/voice-before/voice-before.json"
        final = "candidate/voice-final.json" if two else "candidate/voice-after.json"
        decision = "candidate/voice-combined.json" if two else "candidate/voice-decision.json"
        evidence = {before: f["before"], final: f["after"],
                    decision: f["two" if two else "one"]}
        if two:
            evidence["candidate/voice-middle/voice-after.json"] = f["middle"]
        checks = [{"check": name, "verdict": "PASS"} for name in ("create", "moved", "delete")]
        lifecycles = [f"{name}/voice-synthetic-123-1.json" for name in (("first", "second") if two else ("first",))]
        evidence.update(dict.fromkeys(lifecycles, {"checks": checks}))
        for name, value in evidence.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(value), encoding="utf-8")
        cleanup = root / "cleanup/staging-use-measurement-stop.jsonl"
        cleanup.parent.mkdir()
        correlation = {"environment": "staging", "runId": "123", "runAttempt": 1}
        binding = {"sha": "a" * 40, "deploymentId": "deploy", "epoch": 1}
        use = dict(correlation, **binding, expiresAt=f["expires"], acquiredAt=f["expires"] - 3602000)

        def write_cleanup(start=None, status=None, release=None, acquired=0, omit=None):
            end = f["after"]["capturedAt"]
            prior = dict(use, phase="active", acquiredAt=use["acquiredAt"] + acquired)
            records = [dict(correlation, schema="two-bot-staging-pipeline.v1", action="attempt",
                            operation="finish", observedAt=start if start is not None else end),
                       dict(correlation, **binding, schema="two-bot-staging-use.v1", action="status",
                            observedAt=status if status is not None else end, running=True, use=prior),
                       dict(correlation, **binding, schema="two-bot-staging-use.v1", action="release",
                            observedAt=release if release is not None else end, running=False,
                            use=dict(prior, phase="released"))]
            if omit is not None:
                index, key = omit
                del records[index][key]
            cleanup.write_text("\n".join(map(json.dumps, records)) + "\n", encoding="utf-8")

        output = root / "terminal.json"
        env = {"PATH": os.environ.get("PATH", ""), "PYTHONDONTWRITEBYTECODE": "1",
               "EVIDENCE_DIR": str(root), "SECOND_REQUIRED": second,
               "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1", "STAGING_USE_SHA": binding["sha"],
               "STAGING_USE_DEPLOYMENT": "deploy", "STAGING_USE_EXPIRES_AT": str(f["expires"])}
        env.update({"RESULT_" + job.upper().replace("-", "_"): state for job, state in outcomes(second).items()})

        def run_terminal():
            result = subprocess.run([sys.executable, "scripts/ci/voice_measurement_terminal.py", output],
                                    cwd=checkout, capture_output=True, timeout=5)
            self.assertEqual((result.stdout, result.stderr), (b"", b""))
            return result.returncode

        def assert_verdict(verdict, command=main):
            self.assertEqual(command(), 0 if verdict == "accepted" else 1)
            expected = (json.dumps({"schema": "voice-measurement-terminal.v1", "verdict": verdict}) + "\n").encode()
            self.assertEqual(output.read_bytes(), expected)
            if verdict == "accepted":
                self.assertEqual(command(), 1)  # Never overwrite existing evidence.
                self.assertEqual(output.read_bytes(), expected)
            output.unlink()

        with patch("sys.argv", ["terminal", str(output)]), patch.dict("os.environ", env, clear=True):
            write_cleanup()  # Exact equality at acquisition and finish boundaries.
            grant = root / "grant.json"
            grant_command = ["node", "wrangler/scripts/voice-measurement-metadata.mjs", grant]
            invalid = subprocess.run(grant_command, cwd=checkout, env=dict(env, STAGING_USE_SHA="bad"),
                                     capture_output=True, timeout=10)
            self.assertEqual((invalid.returncode, invalid.stdout, invalid.stderr),
                             (1, b"", b"voice measurement metadata refused\n"))
            self.assertFalse(grant.exists())
            with patch.dict("os.environ", {"SECOND_REQUIRED": ""}):
                assert_verdict("refused", run_terminal)
            valid = subprocess.run(grant_command, cwd=checkout, capture_output=True, timeout=10)
            self.assertEqual((valid.returncode, valid.stdout, valid.stderr), (0, b"", b""))
            metadata = json.dumps(f["metadata"], separators=(",", ":")) + "\n"
            self.assertEqual(grant.read_bytes(), metadata.encode())
            evidence["grant.json"] = f["metadata"]
            assert_verdict("accepted", run_terminal)
            for changes in ({"acquired": 500}, dict.fromkeys(("start", "status", "release"), f["early"]),
                            {"status": f["early"]}, {"release": f["early"]}, {"start": ""},
                            *({"omit": field} for field in ((0, "observedAt"), (1, "runId"), (2, "runId")))):
                with self.subTest(cleanup=changes):
                    write_cleanup(**changes)
                    assert_verdict("refused")
            write_cleanup()
            with patch.dict("os.environ", {"RESULT_VOICE_FIRST": "failure"}):
                assert_verdict("refused", run_terminal)
            for name in (None, *evidence):
                path = root / name if name else cleanup
                with self.subTest(artifact=name):
                    saved = path.read_bytes()
                    path.unlink()
                    assert_verdict("refused")
                    if name == "grant.json" or name in lifecycles:
                        old, new = (b'"deploy"', b'"other"') if name == "grant.json" else (b'"PASS"', b'"FAIL"')
                        path.write_bytes(saved.replace(old, new, 1))
                        assert_verdict("refused")
                    path.write_bytes(saved)
            for path in (root / final, root / decision):
                with self.subTest(input=path.name):
                    data = path.read_bytes()
                    # Test an escaped outer key and a nested key: last-wins would restore the original value.
                    duplicate = (b'{"captured\\u0041t":null,' + data[1:] if path == root / final
                                 else data.replace(b": {", b': {"restTotalDelta":null,', 1))
                    self.assertEqual(json.loads(duplicate), json.loads(data))
                    path.write_bytes(duplicate)
                    assert_verdict("refused", run_terminal)
                    path.write_bytes(data.ljust(65536))
                    assert_verdict("accepted")
                    path.write_bytes(data.ljust(65537))
                    assert_verdict("refused")
                    target = path.with_suffix(".regular")
                    target.write_bytes(data)
                    path.unlink()
                    path.symlink_to(target)
                    assert_verdict("refused")
                    path.unlink()
                    path.mkdir()
                    assert_verdict("refused")
                    path.rmdir()
                    os.mkfifo(path)
                    assert_verdict("refused", run_terminal)
                    path.unlink()
                    path.write_bytes(data)

    def test_candidate_refuses_process_errors(self):
        args = ("false", {"acquiredAt": 100, "cleanupStartedAt": 200}, "candidate", ["before", "after"])
        with patch("voice_measurement_terminal.subprocess.run") as run:
            run.return_value.returncode = 0
            self.assertTrue(candidate_verified(*args))
            self.assertEqual(json.loads(run.call_args.kwargs["input"]), args[1])
            self.assertEqual(run.call_args.kwargs["timeout"], 15)
            self.assertFalse(candidate_verified("true", *args[1:]))
            for error in (subprocess.TimeoutExpired("node", 15), FileNotFoundError()):
                run.side_effect = error
                self.assertFalse(candidate_verified(*args))

    def test_each_unexpected_job_outcome_refuses(self):
        for second in ("true", "false"):
            results = outcomes(second)
            self.assertEqual(terminal_verdict(results, second, True, True), "accepted")
            for job in JOBS:
                for result in ("failure", "cancelled", "", "unknown"):
                    self.assertEqual(terminal_verdict(dict(results, **{job: result}), second, True, True), "refused")
                self.assertEqual(terminal_verdict({k: results[k] for k in JOBS if k != job}, second, True, True), "refused")
            self.assertEqual(terminal_verdict(dict(results, unexpected="success"), second, True, True), "refused")
            for cleanup, candidate in ((False, True), (True, False), (None, True), (True, None)):
                self.assertEqual(terminal_verdict(results, second, cleanup, candidate), "refused")
            self.assertEqual(terminal_verdict(results, "", True, True), "refused")


if __name__ == "__main__":
    unittest.main()

"""Execute real partition-plan, aggregate, merge and HARD-gate shells offline.

These tests do not emulate GitHub's scheduler: they supply its documented
needs/result/selector values to unchanged run: bodies. The workflow still must
use always() for final verdicts, and matrix workers must not mask failures.
The partition runner's minimal execution interface is documented in
test_critical_path_partitions_acceptance.py. Timing and paired real LCOV
acceptance remain separate empirical work, not synthetic assertions here.
"""

import copy
import json
import re
import shlex
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

from test_critical_path_partitions_acceptance import ROWS, planner, workers
from test_critical_path_support import (
    ROOT, Sandbox, assertions_message, coverage_step, resolved_env, workflow,
)


def needs(job):
    value = job.get("needs", [])
    return value if isinstance(value, list) else [value]


def ancestors(job_id, jobs):
    found = set()
    pending = list(needs(jobs[job_id]))
    while pending:
        name = pending.pop()
        if name in found:
            continue
        found.add(name)
        pending.extend(needs(jobs[name]))
    return found


def run_gate(sandbox, job, overrides=None, selectors=None, shell=None):
    results = {name: "success" for name in needs(job)} | (overrides or {})
    selectors = {name: "true" for name in ("rust", "worker", "parity", "supply")} | (selectors or {})
    context = {name: {"result": result, "outputs": selectors} for name, result in results.items()}
    completed = None
    for step in job.get("steps", []):
        if "run" not in step:
            continue
        env = resolved_env(job, step, sandbox)
        for key, value in step.get("env", {}).items():
            match = re.fullmatch(r"\$\{\{\s*needs\.([\w-]+)\.(result|outputs\.([\w-]+))\s*\}\}", str(value))
            if match:
                env[key] = (results.get(match[1], "") if match[2] == "result"
                            else selectors.get(match[3], ""))
            elif re.fullmatch(r"\$\{\{\s*toJSON\(needs\)\s*\}\}", str(value)):
                env[key] = json.dumps(context)
        completed = sandbox.run(shell or step["run"], env)
        if completed.returncode:
            return completed
    if completed is None:
        raise AssertionError("final verdict has no executable aggregate")
    return completed


class PlanEmissionAcceptance(unittest.TestCase):
    def setUp(self):
        self.sandbox = Sandbox(ROWS)
        self.addCleanup(self.sandbox.close)
        self.step = next(s for s in workflow()["coverage-plan"]["steps"] if s.get("id") == "plan")
        self.valid = planner().plan(["Cargo.toml"], 8, event_name="pull_request",
                                    ref="refs/pull/1/merge", metadata=None)
        shutil.copytree(ROOT / "scripts/ci", self.sandbox.root / "scripts/ci")
        # Intercept only the expensive planner CLI. The real YAML matrix emitter
        # and any in-process validation it calls are left untouched.
        self.sandbox.executable("python3", f'''import json, os, sys
from pathlib import Path
if len(sys.argv) > 1 and sys.argv[1] == 'scripts/ci/plan-coverage-shards.py':
    args = sys.argv[2:]
    Path(args[args.index('--output') + 1]).write_text(os.environ['FIXTURE_PLAN'])
else:
    os.execv({sys.executable!r}, [{sys.executable!r}, *sys.argv[1:]])
''')

    def emit(self, value):
        return self.sandbox.run(self.step["run"], {
            "FIXTURE_PLAN": json.dumps(value), "PLAN_BASE": "base", "PLAN_HEAD": "head",
            "PLAN_EVENT": "pull_request", "PLAN_REF": "refs/pull/1/merge",
        }, cwd=self.sandbox.root)

    def test_intact_expanded_plan_emits_exact_nonempty_matrix(self):
        proc = self.emit(self.valid)
        self.assertEqual(proc.returncode, 0, assertions_message(proc))
        values = dict(line.split("=", 1) for line in (self.sandbox.root / "github-output").read_text().splitlines())
        include = json.loads(values["matrix"])["include"]
        got = [leg for row in include for leg in row["legs"].split()]
        self.assertCountEqual(got, self.valid["legs"])
        self.assertEqual(len(got), len(set(got)))
        self.assertEqual(int(values["expected-shards"]), len(include))
        self.assertTrue(all(row["legs"].strip() for row in include))

    def test_missing_duplicate_unknown_or_empty_plan_ownership_cannot_be_published(self):
        variants = {}
        bad = copy.deepcopy(self.valid)
        bad["shards"][0].pop(0)
        variants["missing"] = bad
        bad = copy.deepcopy(self.valid)
        bad["shards"][1].append(bad["shards"][0][0])
        variants["duplicate"] = bad
        bad = copy.deepcopy(self.valid)
        bad["shards"][0][0] = "unknown-critical-path-leg"
        variants["unknown"] = bad
        variants["empty"] = self.valid | {"legs": [], "shards": [[], []]}
        for kind, value in variants.items():
            with self.subTest(kind=kind):
                (self.sandbox.root / "github-output").unlink(missing_ok=True)
                proc = self.emit(value)
                self.assertNotEqual(proc.returncode, 0, f"published {kind} ownership as measured")
                output = self.sandbox.root / "github-output"
                self.assertFalse(output.exists() and "matrix=" in output.read_text())


class FinalGateAcceptance(unittest.TestCase):
    def setUp(self):
        self.sandbox = Sandbox(ROWS)
        self.addCleanup(self.sandbox.close)
        self.jobs = workflow()

    def test_final_check_names_are_unique_and_do_not_become_matrix_contexts(self):
        for name in ("rust tests", "ignored db (runtime)", "patch coverage", "ci-ok"):
            with self.subTest(name=name):
                found = [j for j in self.jobs.values() if j.get("name") == name]
                self.assertEqual(len(found), 1)
                self.assertNotIn("matrix", found[0].get("strategy", {}))
        self.assertIn("always()", self.jobs["ci-ok"].get("if", ""))

    def test_every_partition_worker_reaches_its_strict_stable_final_gate(self):
        for lane in ("rust-tests", "ignored-db-runtime"):
            with self.subTest(lane=lane):
                found = workers(lane)
                self.assertTrue(found, f"{lane} has no separately executable partition worker")
                self.assertIn("always()", self.jobs[lane].get("if", ""))
                self.assertIn(lane, ancestors("ci-ok", self.jobs))
                for worker, _, _ in found:
                    self.assertIn(worker, ancestors(lane, self.jobs))
                    # A DAG path alone is insufficient: poisoning each direct
                    # worker/intermediate result must change the actual verdict.
                for dep in needs(self.jobs[lane]):
                    for state in ("failure", "cancelled", "skipped", ""):
                        with self.subTest(dependency=dep, state=state):
                            proc = run_gate(self.sandbox, self.jobs[lane], {dep: state})
                            self.assertNotEqual(proc.returncode, 0, assertions_message(proc))
                self.assertEqual(run_gate(self.sandbox, self.jobs[lane]).returncode, 0)

    def test_ci_ok_rejects_every_failed_cancelled_missing_and_unexpectedly_skipped_dependency(self):
        job = self.jobs["ci-ok"]
        self.assertEqual(run_gate(self.sandbox, job).returncode, 0)
        for dep in needs(job):
            for state in ("failure", "cancelled", "skipped", ""):
                with self.subTest(dependency=dep, state=state):
                    proc = run_gate(self.sandbox, job, {dep: state})
                    self.assertNotEqual(proc.returncode, 0, dep + " was not fail-closed")

    def test_only_explicit_rust_deselection_permits_rust_skips(self):
        job = self.jobs["ci-ok"]
        rust = {name: "skipped" for name in needs(job)
                if name in ("rust-tests", "ignored-db-runtime", "coverage-plan", "coverage-shard", "patch-coverage")}
        self.assertEqual(run_gate(self.sandbox, job, rust, {"rust": "false"}).returncode, 0)
        for selector in ("", "true"):
            self.assertNotEqual(run_gate(self.sandbox, job, rust, {"rust": selector}).returncode, 0)
        self.assertNotEqual(run_gate(self.sandbox, job, rust | {"job-inputs": "failure"}, {"rust": "false"}).returncode, 0)
        self.assertNotEqual(run_gate(self.sandbox, job, {"worker": "skipped"}, {"rust": "false"}).returncode, 0)

    def test_actual_aggregate_failure_mask_mutation_is_detected(self):
        job = self.jobs["ci-ok"]
        shell = next(s["run"] for s in job["steps"] if "run" in s)
        self.assertIn("sys.exit(1)", shell)
        intact = run_gate(self.sandbox, job, {"coverage-shard": "failure"})
        self.assertNotEqual(intact.returncode, 0)
        mutated = run_gate(self.sandbox, job, {"coverage-shard": "failure"},
                           shell=shell.replace("sys.exit(1)", "sys.exit(0)"))
        with self.assertRaises(AssertionError):
            self.assertNotEqual(mutated.returncode, 0, "mutation survived the fail-closed assertion")

    def test_actual_guarded_replay_failure_mask_mutation_is_detected(self):
        step = coverage_step(self.jobs)
        shell = step["run"]
        match = re.search(r"cov_run\(\)\s*\{.*?\n\}", shell, re.S)
        self.assertIsNotNone(match)
        env = resolved_env(self.jobs["coverage-shard"], step, self.sandbox) | {
            "SHARD_LEGS": " ".join(leg["id"] for leg in planner().FULL_LEGS),
            "SHARD_INDEX": "0", "FAIL_NEEDLE": "starboard_store"}
        intact = self.sandbox.run(shell, env)
        self.assertNotEqual(intact.returncode, 0)
        self.assertTrue(any("starboard_store" in r["argv"] for r in self.sandbox.records()))
        mutant = shell[:match.start()] + match[0].replace("exit 1", "true") + shell[match.end():]
        mutated = self.sandbox.run(mutant, env)
        with self.assertRaises(AssertionError):
            self.assertNotEqual(mutated.returncode, 0, "mutation survived the guarded-replay assertion")


class ReportGateControls(unittest.TestCase):
    def setUp(self):
        self.sandbox = Sandbox(ROWS)
        self.addCleanup(self.sandbox.close)
        dest = self.sandbox.root / "scripts/ci"
        dest.mkdir(parents=True)
        for name in ("merge-coverage-shards.py", "check-patch-coverage.py"):
            shutil.copy2(ROOT / "scripts/ci" / name, dest / name)

    def test_merge_shell_requires_expanded_n_of_n_nonempty_reports(self):
        step = next(s for s in workflow()["patch-coverage"]["steps"] if s.get("name") == "Merge coverage shards")
        directory = self.sandbox.root / "coverage-shards"
        directory.mkdir()
        report = "SF:crates/core/src/lib.rs\nDA:1,1\nend_of_record\n"
        for index in range(8):
            (directory / f"lcov-shard-{index}.info").write_text(report)
        def run():
            return self.sandbox.run(step["run"], {"EXPECTED_SHARDS": "8"}, cwd=self.sandbox.root)
        self.assertEqual(run().returncode, 0)
        missing = directory / "lcov-shard-7.info"
        missing.unlink()
        self.assertNotEqual(run().returncode, 0)
        missing.write_text("")
        self.assertNotEqual(run().returncode, 0)
        missing.write_text(report)
        (directory / "lcov-shard-8.info").write_text(report)
        self.assertNotEqual(run().returncode, 0)

    def test_duplicate_report_is_not_two_independent_partition_reports(self):
        report = self.sandbox.root / "one.info"
        report.write_text("SF:src/lib.rs\nDA:1,1\nend_of_record\n")
        command = [sys.executable, str(ROOT / "scripts/ci/merge-coverage-shards.py"),
                   "--report", str(report), "--report", str(report),
                   "--expected-shards", "2", "--out", str(self.sandbox.root / "union.info")]
        proc = self.sandbox.run(shlex.join(command))
        self.assertNotEqual(proc.returncode, 0, "one report was counted twice as N-of-N")

    def test_real_hard_gate_rejects_seventy_percent_and_accepts_eighty_percent(self):
        root = self.sandbox.root
        def git(*args):
            return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()
        git("init", "-q")
        git("config", "user.name", "Acceptance Fixture")
        git("config", "user.email", "fixture@example.invalid")
        source = root / "src/lib.rs"
        source.parent.mkdir()
        source.write_text("pub fn existing() {}\n")
        git("add", "src/lib.rs")
        git("commit", "-qm", "test: baseline fixture")
        base = git("rev-parse", "HEAD")
        source.write_text("pub fn existing() {}\n" + "".join(f"pub fn added_{i}() {{}}\n" for i in range(10)))
        git("add", "src/lib.rs")
        git("commit", "-qm", "test: changed fixture")
        head = git("rev-parse", "HEAD")
        step = next(s for s in workflow()["patch-coverage"]["steps"] if "check-patch-coverage.py" in s.get("run", ""))
        for covered, expected in ((7, 1), (8, 0), (9, 0)):
            with self.subTest(covered=covered):
                (root / "lcov.info").write_text("SF:src/lib.rs\n" + "".join(
                    f"DA:{i + 2},{int(i < covered)}\n" for i in range(10)) + "end_of_record\n")
                proc = self.sandbox.run(step["run"], {"QUALITY_BASE": base, "QUALITY_HEAD": head}, cwd=root)
                self.assertEqual(proc.returncode, expected, assertions_message(proc))


if __name__ == "__main__":
    unittest.main()

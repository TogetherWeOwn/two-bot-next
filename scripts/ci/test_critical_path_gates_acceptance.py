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

RUNNER = ROOT / "scripts/ci/run-test-partition.py"
EXPECTED_PARTITIONS = {
    "rust-tests": ("unit-bin", "integration-0", "integration-1", "integration-2", "replays"),
    "ignored-db-runtime": ("onboarding", "gateway", "runtime", "cutover", "other"),
}
PROPERTY_OWNERS = {"unit-bin", "integration-0", "integration-1", "integration-2"}


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


def lane_of(job):
    for step in job.get("steps", []):
        lane = (job.get("env", {}) | step.get("env", {})).get("CI_TEST_LANE")
        if lane in EXPECTED_PARTITIONS:
            return lane
    return None


def stage_valid_receipts(sandbox, lane, values=None):
    """Emulate a successful artifact download with exact valid receipts.

    The production aggregator downloads ``test-partition-<lane>-*`` into
    ``test-partition-receipts`` (relative to the workspace) and then runs the
    real ``--verify-receipts`` shell. Offline we stage the same directory under
    the isolated sandbox (``$RUNNER_TEMP`` and ``$GITHUB_WORKSPACE`` both point
    there) with exactly five ``<lane>-<partition>.json`` files.
    """
    target = sandbox.root / "test-partition-receipts"
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True)
    values = values if values is not None else {p: 0.0 for p in EXPECTED_PARTITIONS[lane]}
    for partition, seconds in values.items():
        (target / f"{lane}-{partition}.json").write_text(
            json.dumps({"lane": lane, "partition": partition,
                        "property_seconds": seconds}) + "\n",
            encoding="utf-8")
    return target


def write_receipt_values(directory, lane, values):
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    for partition, seconds in values.items():
        (directory / f"{lane}-{partition}.json").write_text(
            json.dumps({"lane": lane, "partition": partition,
                        "property_seconds": seconds}) + "\n",
            encoding="utf-8")
    return directory


def verify_direct(sandbox, lane, directory):
    """Invoke the real production verifier against an isolated directory."""
    return sandbox.run(shlex.join(
        [sys.executable, str(RUNNER), "--lane", lane, "--verify-receipts", str(directory)]))


def run_gate(sandbox, job, overrides=None, selectors=None, shell=None, receipts="valid"):
    """Execute real aggregate shells with emulated artifact download.

    ``receipts="valid"`` (default) stages five exact zero-duration receipts so
    positive fixtures prove the aggregate path, and poisoned dependencies must
    still fail at the Require step even with valid receipts present.
    ``receipts="preserve"`` leaves a caller-staged ``test-partition-receipts``
    directory untouched for tamper controls. ``receipts=None`` removes the
    directory to prove missing-download rejection.
    """
    lane = lane_of(job)
    if lane is not None:
        dest = sandbox.root / "scripts/ci/run-test-partition.py"
        dest.parent.mkdir(parents=True, exist_ok=True)
        if not dest.exists():
            shutil.copy2(RUNNER, dest)
        target = sandbox.root / "test-partition-receipts"
        if receipts == "valid":
            stage_valid_receipts(sandbox, lane)
        elif receipts is None:
            if target.exists():
                shutil.rmtree(target)
        elif receipts != "preserve":
            raise ValueError(f"unknown receipts fixture {receipts!r}")
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
        completed = sandbox.run(shell or step["run"], env, cwd=sandbox.root)
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

    def test_newline_bearing_reason_is_json_encoded_and_cannot_overwrite_matrix(self):
        evil = copy.deepcopy(self.valid)
        evil["reason"] = 'first line\nmatrix={"include": []}\nexpected-shards=0\nsecond line'
        (self.sandbox.root / "github-output").unlink(missing_ok=True)
        proc = self.emit(evil)
        self.assertEqual(proc.returncode, 0, assertions_message(proc))
        lines = (self.sandbox.root / "github-output").read_text().splitlines()
        # JSON encoding keeps the multiline reason on one output line; a raw
        # write would inject extra matrix=/expected-shards= lines that overwrite
        # the real matrix when GitHub parses GITHUB_OUTPUT.
        self.assertEqual(len(lines), 5, f"reason newline injected output lines: {lines!r}")
        values = dict(line.split("=", 1) for line in lines)
        include = json.loads(values["matrix"])["include"]
        got = [leg for row in include for leg in row["legs"].split()]
        self.assertCountEqual(got, self.valid["legs"])
        self.assertEqual(int(values["expected-shards"]), len(include))
        self.assertGreater(len(include), 0, "injected empty matrix overwrote the real plan")
        self.assertEqual(json.loads(values["reason"]), evil["reason"])


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


class PartitionReceiptVerificationAcceptance(unittest.TestCase):
    """Real verifier and real final-aggregate receipt controls, offline.

    The production runner expects exactly five ``<lane>-<partition>.json``
    files with exactly ``{lane, partition, property_seconds}``; identity must
    match the filename, durations must be finite nonnegative numbers (never
    bool), non-property owners must be zero, and summed Rust properties must be
    strictly below 30 seconds (combined global budget, not per partition).
    Every rejection below is proven twice: once against the real
    ``--verify-receipts`` CLI and once end-to-end through the real aggregator
    ``run:`` shells via :func:`run_gate`.
    """

    def setUp(self):
        self.sandbox = Sandbox(ROWS)
        self.addCleanup(self.sandbox.close)
        self.jobs = workflow()

    def assert_accepts(self, lane, values, name):
        direct_dir = self.sandbox.root / f"direct-{name}"
        write_receipt_values(direct_dir, lane, values)
        proc = verify_direct(self.sandbox, lane, direct_dir)
        self.assertEqual(proc.returncode, 0,
                         f"direct rejected valid {name}\n" + assertions_message(proc))
        target = self.sandbox.root / "test-partition-receipts"
        write_receipt_values(target, lane, values)
        proc = run_gate(self.sandbox, self.jobs[lane], receipts="preserve")
        self.assertEqual(proc.returncode, 0,
                         f"aggregate rejected valid {name}\n" + assertions_message(proc))
        self.assertIn("Verified 5", proc.stdout)

    def assert_rejects(self, lane, populate, name):
        direct_dir = self.sandbox.root / f"direct-{name}"
        populate(direct_dir)
        proc = verify_direct(self.sandbox, lane, direct_dir)
        self.assertNotEqual(proc.returncode, 0, f"direct accepted invalid {name}")
        target = self.sandbox.root / "test-partition-receipts"
        populate(target)
        proc = run_gate(self.sandbox, self.jobs[lane], receipts="preserve")
        self.assertNotEqual(proc.returncode, 0, f"aggregate accepted invalid {name}")
        combined = (proc.stdout or "") + (proc.stderr or "")
        self.assertIn("partition failed", combined,
                      f"aggregate failure for {name} was not receipt rejection: {combined!r}")

    def test_runner_lists_exact_five_expected_partitions(self):
        for lane, expected in EXPECTED_PARTITIONS.items():
            with self.subTest(lane=lane):
                proc = self.sandbox.run(shlex.join(
                    [sys.executable, str(RUNNER), "--lane", lane, "--list"]))
                self.assertEqual(proc.returncode, 0, assertions_message(proc))
                got = [row["partition"] for row in json.loads(proc.stdout)["include"]]
                self.assertEqual(len(got), 5, f"{lane} must have exactly five receipt partitions")
                self.assertCountEqual(got, expected)

    def test_valid_zero_and_29_9_pass_direct_and_aggregate(self):
        cases = [
            ("rust-tests", {p: 0.0 for p in EXPECTED_PARTITIONS["rust-tests"]}, "rust-zero-float"),
            ("rust-tests", {p: 0 for p in EXPECTED_PARTITIONS["rust-tests"]}, "rust-zero-int"),
            ("ignored-db-runtime", {p: 0.0 for p in EXPECTED_PARTITIONS["ignored-db-runtime"]},
             "runtime-zero-float"),
            ("ignored-db-runtime", {p: 0 for p in EXPECTED_PARTITIONS["ignored-db-runtime"]},
             "runtime-zero-int"),
            ("rust-tests", {"unit-bin": 10.0, "integration-0": 10.0, "integration-1": 9.9,
                            "integration-2": 0.0, "replays": 0.0}, "rust-split-29-9"),
            ("rust-tests", {"unit-bin": 29.9, "integration-0": 0.0, "integration-1": 0.0,
                            "integration-2": 0.0, "replays": 0.0}, "rust-single-29-9"),
        ]
        for lane, values, name in cases:
            with self.subTest(case=name):
                total = sum(v for k, v in values.items()
                            if lane == "rust-tests" and k in PROPERTY_OWNERS)
                self.assertLess(total, 30, f"{name} fixture must total <30, got {total}")
                self.assert_accepts(lane, values, name)

    def test_missing_directory_fails_direct_and_aggregate(self):
        for lane in EXPECTED_PARTITIONS:
            with self.subTest(lane=lane):
                missing = self.sandbox.root / f"missing-{lane}"
                if missing.exists():
                    shutil.rmtree(missing)
                proc = verify_direct(self.sandbox, lane, missing)
                self.assertNotEqual(proc.returncode, 0, f"direct accepted missing dir for {lane}")
                proc = run_gate(self.sandbox, self.jobs[lane], receipts=None)
                self.assertNotEqual(proc.returncode, 0, f"aggregate accepted missing dir for {lane}")

    def test_missing_extra_and_bad_filenames_fail(self):
        for lane in EXPECTED_PARTITIONS:
            with self.subTest(lane=lane, case="missing-one"):
                def populate(directory, lane=lane):
                    write_receipt_values(directory, lane,
                                         {p: 0.0 for p in EXPECTED_PARTITIONS[lane][:-1]})
                self.assert_rejects(lane, populate, f"{lane}-missing-one")
            with self.subTest(lane=lane, case="extra-file"):
                def populate(directory, lane=lane):
                    write_receipt_values(directory, lane,
                                         {p: 0.0 for p in EXPECTED_PARTITIONS[lane]})
                    (directory / "extra.json").write_text(json.dumps(
                        {"lane": lane, "partition": "extra", "property_seconds": 0.0}))
                self.assert_rejects(lane, populate, f"{lane}-extra-file")
            with self.subTest(lane=lane, case="bad-filename"):
                def populate(directory, lane=lane):
                    write_receipt_values(directory, lane,
                                         {p: 0.0 for p in EXPECTED_PARTITIONS[lane]})
                    old = directory / f"{lane}-{EXPECTED_PARTITIONS[lane][0]}.json"
                    old.rename(directory / f"{lane}-wrong-name.json")
                self.assert_rejects(lane, populate, f"{lane}-bad-filename")

    def test_duplicate_and_mismatched_identity_fail(self):
        for lane in EXPECTED_PARTITIONS:
            with self.subTest(lane=lane, case="duplicate-identity"):
                def populate(directory, lane=lane):
                    write_receipt_values(directory, lane,
                                         {p: 0.0 for p in EXPECTED_PARTITIONS[lane]})
                    first, second = EXPECTED_PARTITIONS[lane][0], EXPECTED_PARTITIONS[lane][1]
                    (directory / f"{lane}-{second}.json").write_text(json.dumps(
                        {"lane": lane, "partition": first, "property_seconds": 0.0}) + "\n")
                self.assert_rejects(lane, populate, f"{lane}-duplicate-identity")
            with self.subTest(lane=lane, case="unknown-partition"):
                def populate(directory, lane=lane):
                    write_receipt_values(directory, lane,
                                         {p: 0.0 for p in EXPECTED_PARTITIONS[lane]})
                    first = EXPECTED_PARTITIONS[lane][0]
                    (directory / f"{lane}-{first}.json").write_text(json.dumps(
                        {"lane": lane, "partition": "unknown-partition",
                         "property_seconds": 0.0}) + "\n")
                self.assert_rejects(lane, populate, f"{lane}-unknown-partition")
            with self.subTest(lane=lane, case="lane-mismatch"):
                def populate(directory, lane=lane):
                    write_receipt_values(directory, lane,
                                         {p: 0.0 for p in EXPECTED_PARTITIONS[lane]})
                    wrong = "ignored-db-runtime" if lane == "rust-tests" else "rust-tests"
                    first = EXPECTED_PARTITIONS[lane][0]
                    (directory / f"{lane}-{first}.json").write_text(json.dumps(
                        {"lane": wrong, "partition": first, "property_seconds": 0.0}) + "\n")
                self.assert_rejects(lane, populate, f"{lane}-lane-mismatch")

    def test_bad_schema_fails(self):
        for lane in EXPECTED_PARTITIONS:
            first = EXPECTED_PARTITIONS[lane][0]
            variants = {
                "extra-key": json.dumps({"lane": lane, "partition": first,
                                         "property_seconds": 0.0, "extra": 1}) + "\n",
                "missing-key": json.dumps({"lane": lane, "partition": first}) + "\n",
                "not-dict": json.dumps([]) + "\n",
                "invalid-json": '{"lane": ',
            }
            for kind, raw in variants.items():
                with self.subTest(lane=lane, case=kind):
                    def populate(directory, lane=lane, first=first, raw=raw):
                        write_receipt_values(directory, lane,
                                             {p: 0.0 for p in EXPECTED_PARTITIONS[lane]})
                        (directory / f"{lane}-{first}.json").write_text(raw)
                    self.assert_rejects(lane, populate, f"{lane}-schema-{kind}")

    def test_nonfinite_negative_bool_and_string_durations_fail(self):
        for lane in EXPECTED_PARTITIONS:
            first = EXPECTED_PARTITIONS[lane][0]
            variants = {
                "nan": float("nan"),
                "inf": float("inf"),
                "negative": -1,
                "negative-float": -0.5,
                "bool-true": True,
                "bool-false": False,
                "numeric-string": "0",
                "numeric-string-float": "29.9",
            }
            for kind, bad in variants.items():
                with self.subTest(lane=lane, case=kind):
                    def populate(directory, lane=lane, first=first, bad=bad):
                        values = {p: 0.0 for p in EXPECTED_PARTITIONS[lane]}
                        values[first] = bad
                        write_receipt_values(directory, lane, values)
                    # False is numerically zero but must still fail as bool.
                    self.assert_rejects(lane, populate, f"{lane}-duration-{kind}")

    def test_nonproperty_time_fails(self):
        with self.subTest(lane="rust-tests", case="replays-nonzero"):
            def populate(directory):
                values = {p: 0.0 for p in EXPECTED_PARTITIONS["rust-tests"]}
                values["replays"] = 0.1
                write_receipt_values(directory, "rust-tests", values)
            self.assert_rejects("rust-tests", populate, "rust-nonproperty-replays")
        with self.subTest(lane="ignored-db-runtime", case="any-nonzero"):
            def populate(directory):
                values = {p: 0.0 for p in EXPECTED_PARTITIONS["ignored-db-runtime"]}
                values["onboarding"] = 0.1
                write_receipt_values(directory, "ignored-db-runtime", values)
            self.assert_rejects("ignored-db-runtime", populate, "runtime-nonproperty-any")

    def test_exactly_30_and_split_over_30_fail_as_combined_global_budget(self):
        # Each split part is below 30 on its own; only the combined sum proves
        # the budget is global, not per partition.
        cases = [
            ({"unit-bin": 30.0, "integration-0": 0.0, "integration-1": 0.0,
              "integration-2": 0.0, "replays": 0.0}, "rust-exactly-30-single"),
            ({"unit-bin": 10, "integration-0": 10, "integration-1": 10,
              "integration-2": 0, "replays": 0}, "rust-exactly-30-split"),
            ({"unit-bin": 8, "integration-0": 8, "integration-1": 8,
              "integration-2": 8, "replays": 0}, "rust-split-32"),
            ({"unit-bin": 10.0, "integration-0": 10.0, "integration-1": 10.0,
              "integration-2": 0.1, "replays": 0.0}, "rust-split-30-1"),
        ]
        for values, name in cases:
            with self.subTest(case=name):
                total = sum(v for k, v in values.items() if k in PROPERTY_OWNERS)
                self.assertGreaterEqual(total, 30, f"{name} fixture must total >=30, got {total}")
                if "single" not in name:
                    self.assertTrue(all(v < 30 for v in values.values()),
                                    f"{name} must keep every part below 30 to prove global summing")
                def populate(directory, values=values):
                    write_receipt_values(directory, "rust-tests", values)
                self.assert_rejects("rust-tests", populate, name)

    def test_poisoned_dependency_fails_at_require_step_with_valid_receipts(self):
        for lane in ("rust-tests", "ignored-db-runtime"):
            with self.subTest(lane=lane):
                proc = run_gate(self.sandbox, self.jobs[lane])
                self.assertEqual(proc.returncode, 0, assertions_message(proc))
                self.assertIn("Verified 5", proc.stdout)
                for dep in needs(self.jobs[lane]):
                    with self.subTest(dependency=dep):
                        proc = run_gate(self.sandbox, self.jobs[lane], {dep: "failure"})
                        self.assertNotEqual(proc.returncode, 0,
                                            f"{dep} poison passed with valid receipts")
                        self.assertIn("required test partition did not succeed", proc.stdout,
                                      "poison failure was not Require-step rejection with valid receipts")


class PartitionArtifactBindingAcceptance(unittest.TestCase):
    def test_download_is_current_run_and_uploads_are_strict_success_only(self):
        jobs = workflow()
        for lane, worker in (("rust-tests", "rust-tests-partition"),
                             ("ignored-db-runtime", "ignored-db-runtime-partition")):
            with self.subTest(lane=lane):
                job = jobs[worker]
                uploads = [s for s in job.get("steps", [])
                           if str(s.get("uses", "")).startswith("actions/upload-artifact")]
                self.assertEqual(len(uploads), 1, f"{worker} must have exactly one receipt upload")
                up = uploads[0]
                self.assertEqual(up.get("with", {}).get("name"),
                                 f"test-partition-{lane}-" + "${{ matrix.partition }}")
                self.assertEqual(up.get("with", {}).get("path"),
                                 "${{ runner.temp }}/test-partition-receipts/" + f"{lane}-" +
                                 "${{ matrix.partition }}.json")
                self.assertEqual(up.get("with", {}).get("if-no-files-found"), "error")
                condition = str(up.get("if", "success()")).removeprefix("${{").removesuffix("}}").strip()
                self.assertEqual(condition, "success()", f"{worker} upload must run only on success")
                agg = jobs[lane]
                downloads = [s for s in agg.get("steps", [])
                             if str(s.get("uses", "")).startswith("actions/download-artifact")]
                self.assertEqual(len(downloads), 1, f"{lane} must download exactly once")
                dl = downloads[0].get("with", {})
                self.assertEqual(dl.get("pattern"), f"test-partition-{lane}-*")
                self.assertEqual(dl.get("path"), "test-partition-receipts")
                self.assertTrue(dl.get("merge-multiple"))
                self.assertNotIn("run-id", dl, "receipts must bind the current run, not a cross-run id")
                self.assertNotIn("github-token", dl)
                verifies = [s for s in agg.get("steps", []) if "verify-receipts" in s.get("run", "")]
                self.assertEqual(len(verifies), 1, f"{lane} must verify exactly once")
                self.assertEqual(verifies[0].get("env", {}).get("CI_TEST_LANE"), lane)
                self.assertIn("--verify-receipts test-partition-receipts", verifies[0]["run"])


class PartitionReceiptExecutionAcceptance(unittest.TestCase):
    """Real partition execution writes exact receipts only on success.

    Cargo itself stays doubled at the metadata/compilation boundary (the
    shared :class:`Sandbox` recorder); the partition runner, its receipt
    writer and the verifier are the real production code.
    """

    def setUp(self):
        self.sandbox = Sandbox(ROWS)
        self.addCleanup(self.sandbox.close)

    def run_partition(self, lane, partition, env=None):
        return self.sandbox.run(shlex.join(
            [sys.executable, str(RUNNER), "--lane", lane, "--partition", partition]), env)

    def test_successful_execution_writes_exact_receipts_that_verify(self):
        for lane in EXPECTED_PARTITIONS:
            with self.subTest(lane=lane):
                target = self.sandbox.root / "test-partition-receipts"
                if target.exists():
                    shutil.rmtree(target)
                for partition in EXPECTED_PARTITIONS[lane]:
                    proc = self.run_partition(lane, partition)
                    self.assertEqual(proc.returncode, 0,
                                     f"{lane}/{partition}\n" + assertions_message(proc))
                    path = target / f"{lane}-{partition}.json"
                    self.assertTrue(path.is_file(), f"missing receipt for {lane}/{partition}")
                    receipt = json.loads(path.read_text(encoding="utf-8"))
                    self.assertEqual(set(receipt), {"lane", "partition", "property_seconds"})
                    self.assertEqual(receipt["lane"], lane)
                    self.assertEqual(receipt["partition"], partition)
                    seconds = receipt["property_seconds"]
                    self.assertNotIsInstance(seconds, bool)
                    self.assertIsInstance(seconds, (int, float))
                    self.assertGreaterEqual(seconds, 0)
                    if lane != "rust-tests" or partition not in PROPERTY_OWNERS:
                        self.assertEqual(seconds, 0)
                    self.assertTrue(path.read_text(encoding="utf-8").endswith("\n"))
                self.assertEqual({p.name for p in target.iterdir()},
                                 {f"{lane}-{p}.json" for p in EXPECTED_PARTITIONS[lane]})
                proc = verify_direct(self.sandbox, lane, target)
                self.assertEqual(proc.returncode, 0, assertions_message(proc))
                self.assertIn("Verified 5", proc.stdout)

    def test_failed_execution_writes_no_receipt(self):
        cases = [("rust-tests", "unit-bin"), ("rust-tests", "replays"),
                 ("ignored-db-runtime", "onboarding")]
        for lane, partition in cases:
            with self.subTest(lane=lane, partition=partition):
                target = self.sandbox.root / "test-partition-receipts"
                if target.exists():
                    shutil.rmtree(target)
                target.mkdir()
                stale = target / f"{lane}-{partition}.json"
                stale.write_text(json.dumps({"lane": lane, "partition": partition,
                                              "property_seconds": 0.0}) + "\n")
                self.assertTrue(stale.is_file())
                proc = self.run_partition(lane, partition, {"FAIL_NEEDLE": "--locked"})
                self.assertNotEqual(proc.returncode, 0, f"{lane}/{partition} swallowed cargo failure")
                missing = target / f"{lane}-{partition}.json"
                self.assertFalse(missing.exists(), f"failed {lane}/{partition} exported a receipt")
        with self.subTest(case="empty-inventory"):
            empty = Sandbox([])
            self.addCleanup(empty.close)
            proc = empty.run(shlex.join(
                [sys.executable, str(RUNNER), "--lane", "rust-tests", "--partition", "unit-bin"]))
            self.assertNotEqual(proc.returncode, 0, "empty inventory was reported as measured")
            self.assertFalse((empty.root / "test-partition-receipts" / "rust-tests-unit-bin.json").exists())


if __name__ == "__main__":
    unittest.main()

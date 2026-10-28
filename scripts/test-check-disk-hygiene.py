"""Lock disk hygiene to executing Rust CI workers, with offline process doubles.

Two consecutive main `check` failures died in the `cargo test --workspace
--test '*'` link step on hosted overflow runners: the job-container overlay
ran out of space (98 MB left), then `rust-lld` bus error. Unit tests passed
in the same jobs; infra disk-fit, not a code regression.

The old single `check` job is now parallel lanes (`check` lints,
`rust-tests-partition`, `coverage-shard`, `ignored-db-stores` and
`ignored-db-runtime-partition` test); each execution worker keeps the guards:
- every lane pins dev/test debuginfo to `line-tables-only`: smaller linked
  binaries, backtraces stay useful.
- every lane disables dev/test incremental compilation: a cold CI link never
  reuses incremental artifacts, so don't write them.
- every lane's job-container prerequisites step removes `/var/lib/apt/lists`:
  the package lists stay on the overlay otherwise.
- in the `check` lane the `cargo-deny` step runs before the toolchain install
  and every compile/link step: the deny image ships its own toolchain and
  rustup-syncs the pinned one (a second ~1 GB toolchain download), so it
  must run while the disk is at its freest (CHANGES on PR #232, 07:31Z).
- the Rust partition workers still execute the full integration graph,
  including future targets, with the same features, replays and isolation:
  coverage must not be narrowed as a substitute for freeing disk.
- every lane carries a disk-headroom guard right after its prerequisites
  step: it logs `df` evidence and fails fast below a 1 GiB floor instead of
  dying mid-suite (run 39209332791: ignored-db-stores died with Postgres
  `PANIC: could not write to file "pg_wal/xlogtemp.*": No space left on
  device` on the 1 GiB tmpfs).
- every Postgres lane reports storage headroom right after verify: per-DB
  sizes, WAL bytes and the bounded WAL budget, so the next incident has a
  prepare-time baseline. max_wal_size is a soft target, not a ceiling, so
  every Postgres lane also reports post-suite (always-runs) to bracket peak
  WAL for the next ENOSPC triage.

Toolchain ordering guard: pip 23 probes `rustc --version` from the workspace
with a 0.5 s timeout for its user agent. If the pinned toolchain still lacks
the components named in rust-toolchain.toml, the rustup proxy starts
installing them and the timeout kills it half-way, leaving untracked
`bin/cargo-fmt`; the later `cargo fmt` then fails with a file conflict in
roughly half the runs. Every lane that runs pip installs the components
before its first pip run.
"""

import json
import re
import sys
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/check.yml"
sys.path.insert(0, str(ROOT / "scripts/ci"))

from test_critical_path_partitions_acceptance import (
    BASELINE, evolving_inventory, list_partitions, matrix_entries,
)
from test_critical_path_gates_acceptance import run_gate
from test_critical_path_support import Sandbox, assertions_message, ledger, resolved_env


def job_body_lines(text, job_key):
    """Raw lines of one top-level job block (blank lines dropped)."""
    lines = text.splitlines()
    start = next(
        i for i, line in enumerate(lines)
        if re.match(rf"^  {re.escape(job_key)}:\s*$", line)
    )
    body = []
    for line in lines[start + 1:]:
        if not line.strip():
            continue
        if re.match(r"^  \S", line):
            break
        body.append(line)
    return body


def child_block(body, header, header_indent):
    """Lines under `header` until a line at or above its indent."""
    start = next(i for i, line in enumerate(body) if line.strip() == header)
    depth = len(body[start]) - len(body[start].lstrip())
    assert depth == header_indent, f"{header} moved (indent {depth})"
    taken = []
    for line in body[start + 1:]:
        if len(line) - len(line.lstrip()) <= depth:
            break
        taken.append(line)
    return taken


def step_body(body, step_name):
    """Raw text of one `- name:` step until the next same-level `- ` item."""
    start = next(
        i for i, line in enumerate(body)
        if line.strip() == f"- name: {step_name}"
    )
    taken = []
    for line in body[start + 1:]:
        if re.match(r"^      - ", line):
            break
        taken.append(line)
    return "\n".join(taken)


LANES = ("check", "rust-tests-partition", "coverage-shard", "ignored-db-stores", "ignored-db-runtime-partition")
PARTITION_LANES = {
    "rust-tests-partition": "rust-tests",
    "ignored-db-runtime-partition": "ignored-db-runtime",
}
PREREQUISITES = "Job-container prerequisites"
GUARD = "Guard runner disk headroom"
VERIFY = "Verify disposable Postgres defaults"
REPORT = "Report Postgres storage headroom"
POST_REPORT = "Report Postgres storage headroom (post-suite)"


def step_names(body):
    return [
        line.strip()[len("- name: "):]
        for line in body
        if line.strip().startswith("- name: ")
    ]


class CheckDiskHygieneTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.jobs = {lane: job_body_lines(cls.text, lane) for lane in LANES}
        cls.envs = {lane: child_block(body, "env:", 4) for lane, body in cls.jobs.items()}
        cls.specs = yaml.safe_load(cls.text)["jobs"]

    def assertEnv(self, lane, key, value):
        line = next(
            (line for line in self.envs[lane] if line.strip().startswith(f"{key}:")),
            None,
        )
        self.assertIsNotNone(line, f"{lane} job env must pin {key}")
        self.assertEqual(line.split(":", 1)[1].strip().strip("'\""), value)

    def test_debuginfo_is_line_tables_only(self):
        for lane in LANES:
            with self.subTest(lane=lane):
                self.assertEnv(lane, "CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
                self.assertEnv(lane, "CARGO_PROFILE_TEST_DEBUG", "line-tables-only")

    def test_incremental_compilation_off(self):
        for lane in LANES:
            with self.subTest(lane=lane):
                self.assertEnv(lane, "CARGO_PROFILE_DEV_INCREMENTAL", "false")
                self.assertEnv(lane, "CARGO_PROFILE_TEST_INCREMENTAL", "false")

    def test_prerequisites_drop_apt_lists(self):
        for lane, body in self.jobs.items():
            with self.subTest(lane=lane):
                name = next(name for name in step_names(body) if name.startswith(PREREQUISITES))
                self.assertIn("rm -rf /var/lib/apt/lists", step_body(body, name))

    def test_integration_coverage_not_narrowed(self):
        # Execute the real planner/matrix/runner with Cargo doubled below the
        # selection boundary. The immutable pre-partition ledger independently
        # specifies targets, features, multiplicities, serial/quarantine bounds
        # and environments; an echo, comment or arbitrary Cargo marker has no
        # selected executions and cannot satisfy it. Future targets and a new
        # workspace member are added without editing the frozen recording.
        rows = evolving_inventory()
        sandbox = Sandbox(rows)
        self.addCleanup(sandbox.close)
        for worker, lane in PARTITION_LANES.items():
            with self.subTest(worker=worker):
                job = self.specs[worker]
                dispatch = [s for s in job["steps"] if s.get("id") == "run_partition"]
                self.assertEqual(len(dispatch), 1, "worker lost its executable dispatch")
                step = dispatch[0]
                expected = list_partitions(self, sandbox, lane)
                entries = matrix_entries(self, sandbox, job, self.specs)
                self.assertEqual(entries, expected, "workflow disconnected or narrowed its planner")
                executions = []
                for entry in entries:
                    sandbox.clear()
                    env = resolved_env(job, step, sandbox, entry)
                    self.assertEqual(env.get("CI_TEST_LANE"), lane)
                    self.assertEqual(env.get("CI_TEST_PARTITION"), entry["partition"])
                    proc = sandbox.run(step["run"], env)
                    self.assertEqual(proc.returncode, 0, assertions_message(proc))
                    records = sandbox.records()
                    self.assertTrue(records, "partition mention did not execute any tests")
                    executions.extend(records)
                self.assertEqual(
                    ledger(executions, rows, environments=True),
                    ledger(BASELINE["lanes"][lane], rows, environments=True),
                    "partition dispatch lost or changed baseline executions",
                )

    def test_guarded_workers_feed_strict_stable_final_checks(self):
        sandbox = Sandbox()
        self.addCleanup(sandbox.close)
        for worker, lane in PARTITION_LANES.items():
            with self.subTest(worker=worker):
                job = self.specs[worker]
                self.assertEqual(job["needs"], ["job-inputs", "test-partition-plan"])
                self.assertEqual(job.get("if"), "needs.job-inputs.outputs.rust != 'false'")
                self.assertFalse(job.get("continue-on-error", False))
                self.assertFalse(job["strategy"]["fail-fast"])
                dispatch = [s for s in job["steps"] if s.get("id") == "run_partition"]
                self.assertEqual(len(dispatch), 1)
                env = job.get("env", {}) | dispatch[0].get("env", {})
                self.assertEqual(env.get("CI_TEST_LANE"), lane)
                self.assertEqual(env.get("CI_TEST_PARTITION"), "${{ matrix.partition }}")
                self.assertEqual(dispatch[0].get("if", "success()"), "success()")
                self.assertFalse(dispatch[0].get("continue-on-error", False))
                self.assertIn("agent-testdb", job.get("services", {}))

                final = self.specs[lane]
                self.assertEqual(final["needs"], ["job-inputs", worker])
                self.assertEqual(final.get("if"), "always() && needs.job-inputs.outputs.rust != 'false'")
                self.assertFalse(final.get("continue-on-error", False))
                self.assertNotIn("services", final)
                self.assertNotIn("matrix", final.get("strategy", {}))
                self.assertNotIn("cargo test", "\n".join(job_body_lines(self.text, lane)))
                download = [s for s in final["steps"]
                            if s.get("uses", "").startswith("actions/download-artifact@")]
                self.assertEqual(len(download), 1)
                self.assertEqual(download[0]["with"], {
                    "pattern": f"test-partition-{lane}-*", "path": "test-partition-receipts",
                    "merge-multiple": True,
                })
                for step in final["steps"]:
                    self.assertEqual(step.get("if", "success()"), "success()")
                    self.assertFalse(step.get("continue-on-error", False))
                self.assertEqual(run_gate(sandbox, final).returncode, 0)
                for outcome in ("failure", "cancelled", "skipped", ""):
                    self.assertNotEqual(run_gate(sandbox, final, {worker: outcome}).returncode, 0)
                self.assertNotEqual(run_gate(sandbox, final, receipts=None).returncode, 0)
                self.assertIn(lane, self.specs["ci-ok"]["needs"])
                self.assertNotEqual(run_gate(sandbox, self.specs["ci-ok"], {lane: "failure"}).returncode, 0)

    def test_deny_runs_before_toolchain_and_compiles(self):
        names = step_names(self.jobs["check"])
        self.assertIn("cargo-deny", names)
        deny_at = names.index("cargo-deny")
        for heavy in (
            "Install Rust toolchain",
            "cargo fmt --check",
            "cargo clippy -D warnings",
        ):
            self.assertIn(heavy, names, f"expected step {heavy!r} in the check lane")
            self.assertLess(
                deny_at, names.index(heavy),
                f"cargo-deny must run before {heavy!r} (disk is freest early)",
            )
        # The test lanes never run cargo-deny: it belongs to the lint lane.
        for lane in LANES[1:]:
            self.assertNotIn("cargo-deny", step_names(self.jobs[lane]))

    def test_disk_guard_follows_prerequisites_in_every_lane(self):
        guards = []
        for lane, body in self.jobs.items():
            with self.subTest(lane=lane):
                names = step_names(body)
                prereq = next(name for name in names if name.startswith(PREREQUISITES))
                self.assertIn(GUARD, names, f"missing step {GUARD!r} in {lane}")
                self.assertEqual(
                    names.index(GUARD), names.index(prereq) + 1,
                    f"{GUARD} must run right after prerequisites ({lane}), "
                    "before the toolchain install and cache restore",
                )
                guard = step_body(body, GUARD)
                spec = next(s for s in self.specs[lane]["steps"] if s.get("name") == GUARD)
                self.assertEqual(spec.get("if", "success()"), "success()")
                self.assertFalse(spec.get("continue-on-error", False))
                self.assertIn("shell: bash", guard)
                self.assertIn("df -h", guard)
                self.assertIn("floor=1073741824", guard)
                self.assertIn("::error::runner disk exhausted", guard)
                self.assertIn("Infra flake: rerun the job.", guard)
                # Pin the rejection path: floor plus error text alone pass if
                # the guard stops comparing free_bytes. The executed rejection
                # test lives in scripts/ci/test_workflows.py across all six
                # lanes; here require the comparison fragments and identical
                # bodies so a single-lane edit fails offline.
                self.assertIn('for mount in / "$GITHUB_WORKSPACE"', guard)
                self.assertIn("df -B1 --output=avail", guard)
                self.assertIn('if [ "$free_bytes" -lt "$floor" ]', guard)
                self.assertIn("exit 1", guard)
                guards.append(guard)
        self.assertEqual(len(set(guards)), 1, "the lanes' disk guards drifted apart")

    def test_postgres_headroom_report_follows_verify(self):
        for lane, body in self.jobs.items():
            with self.subTest(lane=lane):
                names = step_names(body)
                if lane == "check":
                    self.assertNotIn(REPORT, names, "the lint lane has no Postgres service")
                    self.assertNotIn(POST_REPORT, names, "the lint lane has no Postgres service")
                    continue
                self.assertIn(REPORT, names, f"missing step {REPORT!r} in {lane}")
                self.assertEqual(
                    names.index(REPORT), names.index(VERIFY) + 1,
                    f"{REPORT} must run right after verify ({lane})",
                )
                report = step_body(body, REPORT)
                self.assertIn("ON_ERROR_STOP=1", report)
                self.assertIn("pg_database_size", report)
                self.assertIn("pg_ls_waldir()", report)
                self.assertIn("max_wal_size", report)
                self.assertIn("min_wal_size", report)
                # Post-suite bracket: max_wal_size is a soft target, so the
                # pre-suite report cannot show peak WAL. The post-suite report
                # always runs after the suite for the next ENOSPC triage.
                self.assertIn(POST_REPORT, names, f"missing step {POST_REPORT!r} in {lane}")
                post = step_body(body, POST_REPORT)
                self.assertIn("if: always()", post)
                self.assertIn("ON_ERROR_STOP=1", post)
                self.assertIn("pg_database_size", post)
                self.assertIn("pg_ls_waldir()", post)
                self.assertIn("max_wal_size", post)
                self.assertIn("min_wal_size", post)
                job = self.specs[lane]
                self.assertIn("agent-testdb", job.get("services", {}))
                steps = job["steps"]
                if lane in PARTITION_LANES:
                    # This is the executable dispatch independently replayed
                    # above, not a step name or a script mention in trivia.
                    test_steps = [i for i, s in enumerate(steps) if s.get("id") == "run_partition"]
                    self.assertEqual(len(test_steps), 1)
                elif lane == "coverage-shard":
                    test_steps = [i for i, s in enumerate(steps) if "SHARD_LEGS" in s.get("env", {})]
                    self.assertEqual(len(test_steps), 1)
                else:
                    # Direct Cargo commands only: echo/comment/env strings
                    # containing "cargo test" are not executable suite steps.
                    test_steps = [i for i, s in enumerate(steps) if re.search(
                        r"(?m)^\s*(?:env(?:\s+-u\s+\w+)*\s+)?cargo\s+(?:test|nextest)\s",
                        s.get("run", ""),
                    )]
                self.assertTrue(test_steps, "worker has no executable suite dispatch")
                positions = {s.get("name"): i for i, s in enumerate(steps)}
                self.assertLess(positions[REPORT], min(test_steps))
                self.assertGreater(positions[POST_REPORT], max(test_steps))
                self.assertEqual(steps[positions[POST_REPORT]].get("if"), "always()")

    def test_storage_reports_execute_exact_queries_and_fail_closed(self):
        sandbox = Sandbox()
        self.addCleanup(sandbox.close)
        log = sandbox.root / "headroom-calls.jsonl"
        sandbox.executable("psql", '''import json, os, sys
with open(os.environ["HEADROOM_LOG"], "a") as out:
    out.write(json.dumps(sys.argv[1:]) + "\\n")
raise SystemExit(int(os.environ.get("PSQL_EXIT", "0")))
''')
        expected = [
            "postgres://agent_test:@agent-testdb:5432/agent_test", "-X", "-w", "-v", "ON_ERROR_STOP=1",
            "-c", "SELECT datname AS database, pg_size_pretty(pg_database_size(datname)) AS size FROM pg_database WHERE datistemplate = false ORDER BY 1",
            "-c", "SELECT pg_size_pretty(COALESCE(sum(size), 0)::bigint) AS wal_bytes, count(*) AS wal_segments FROM pg_ls_waldir()",
            "-c", "SELECT name, setting FROM pg_settings WHERE name IN ('data_directory', 'max_wal_size', 'min_wal_size') ORDER BY 1",
        ]
        for lane in LANES[1:]:
            for name in (REPORT, POST_REPORT):
                with self.subTest(lane=lane, report=name):
                    step = next(s for s in self.specs[lane]["steps"] if s.get("name") == name)
                    log.unlink(missing_ok=True)
                    env = resolved_env(self.specs[lane], step, sandbox) | {"HEADROOM_LOG": str(log)}
                    proc = sandbox.run(step["run"], env)
                    self.assertEqual(proc.returncode, 0, assertions_message(proc))
                    self.assertEqual([json.loads(line) for line in log.read_text().splitlines()], [expected])
                    self.assertNotEqual(sandbox.run(step["run"], env | {"PSQL_EXIT": "43"}).returncode, 0)

    def test_toolchain_components_installed_before_first_pip_run(self):
        install = "Install pinned Rust toolchain components"
        for lane, body in self.jobs.items():
            with self.subTest(lane=lane):
                first_pip = next(
                    (i for i, line in enumerate(body)
                     if re.search(r"(?:-m pip\b|^\s*pip3? )", line)),
                    None,
                )
                if first_pip is None:
                    continue  # lane never invokes pip; nothing to guard
                install_at = next(
                    (i for i, line in enumerate(body)
                     if line.strip() == f"- name: {install}"),
                    None,
                )
                self.assertIsNotNone(install_at, f"missing step {install!r} in {lane}")
                self.assertLess(
                    install_at, first_pip,
                    f"pip probes `rustc --version`; finish the toolchain install first ({lane})",
                )
                step = step_body(body, install)
                self.assertRegex(step, r"(?m)^\s+run: rustup toolchain install --no-self-update$")
                self.assertRegex(step, r"(?m)^\s+if: needs\.job-inputs\.outputs\.rust != 'false'$")


if __name__ == "__main__":
    unittest.main()

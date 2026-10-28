"""Offline pins for the required-check contract CONTRIBUTING states.

GitHub leaves a required check pending forever when the workflow that owns it
never starts (a `paths`/`branches` filter on the trigger), and counts a skipped
job as passing. Each required context must be exactly one job in an unfiltered
pull_request workflow. The single `ci-ok` aggregate must run, and fail, when
job selection or a required dependency fails.
"""

import os
from pathlib import Path
import re
import subprocess
import sys
import textwrap
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
REQUIRED = {"ci-ok", "gitleaks", "pr-lint"}
TRIGGER_FILTERS = {"paths", "paths-ignore", "branches", "branches-ignore"}
# Only the advisory image smoke and the aggregate itself sit outside ci-ok.
# The smoke's source-level exemption carries the reason; both selectors gate.
NOT_AGGREGATED = {"container", "ci-ok"}
AGGREGATORS = ("ci-ok",)


def load_workflows():
    return {path.name: yaml.load(path.read_text(), Loader=yaml.BaseLoader)
            for path in (ROOT / ".github/workflows").glob("*.y*ml")}


def documented_required_checks():
    line = next(line for line in (ROOT / "CONTRIBUTING.md").read_text().splitlines()
                if "are required checks on `main`" in line)
    return set(re.findall(r"`([^`]+)`", line.split("are required checks")[0]))


class RequiredChecksReportTests(unittest.TestCase):
    def setUp(self):
        self.workflows = load_workflows()

    def test_contributing_names_the_required_checks(self):
        self.assertEqual(documented_required_checks(), REQUIRED)

    def test_each_required_context_is_one_job_with_an_unfiltered_trigger(self):
        names = {}
        for name, workflow in self.workflows.items():
            for job_id, job in workflow["jobs"].items():
                names.setdefault(job.get("name", job_id), []).append(name)
        for context in sorted(REQUIRED):
            with self.subTest(context=context):
                self.assertEqual(len(names.get(context, [])), 1, f"{context}: needs exactly one job")
                workflow_name = names[context][0]
                trigger = (self.workflows[workflow_name].get("on") or {}).get("pull_request")
                self.assertIsNotNone(trigger, f"{workflow_name} must run on pull_request")
                filters = set(trigger) if isinstance(trigger, dict) else set()
                self.assertFalse(filters & TRIGGER_FILTERS,
                                 f"{workflow_name}: a filtered trigger leaves {context} pending")

    def test_lint_runs_and_fails_when_job_selection_fails(self):
        check = self.workflows["check.yml"]["jobs"]["check"]
        self.assertIn("always()", check["if"])
        self.assertIn("job-inputs", check["needs"])
        guard = next(step for step in check["steps"]
                     if step.get("name") == "require job inputs selection to pass")
        self.assertEqual(guard["if"], "needs.job-inputs.result != 'success'")
        self.assertEqual(guard["run"], "exit 1")

    def test_ci_ok_covers_selectors_and_path_filtered_jobs(self):
        agg = self.workflows["check.yml"]["jobs"]["ci-ok"]
        self.assertEqual(agg["name"], "ci-ok")
        self.assertIn("always()", agg["if"])
        for job in ("job-inputs", "container-inputs", "supply-chain", "check", "rust-tests",
                    "test-partition-plan", "rust-tests-partition",
                    "ignored-db-runtime-partition",
                    "coverage-plan", "coverage-shard", "patch-coverage",
                    "ignored-db-stores", "ignored-db-runtime", "worker",
                    "parity-docs", "self-role-store", "community-db",
                    "feeds-db", "tickets-postgres", "moderation-db", "fuzz-compile",
                    "nextest"):
            self.assertIn(job, agg["needs"], job)
        # Advisory image smoke remains off the image-build critical path.
        self.assertNotIn("container", agg["needs"])
        self.assertIn("ci-ok", agg["runs-on"])
        body = "\n".join(step.get("run", "") for step in agg["steps"])
        for marker in ("JOB_INPUTS_RESULT", "CONTAINER_INPUTS_RESULT", "SUPPLY_CHAIN_RESULT",
                       "CHECK_RESULT", "WORKER_RESULT", "PARITY_DOCS_RESULT",
                       "SELF_ROLE_RESULT", "COMMUNITY_DB_RESULT",
                       "FEEDS_DB_RESULT", "TICKETS_RESULT",
                       "MODERATION_DB_RESULT", "RUST_TESTS_RESULT",
                       "TEST_PARTITION_PLAN_RESULT", "RUST_TEST_PARTITION_RESULT",
                       "RUNTIME_PARTITION_RESULT",
                       "COVERAGE_PLAN_RESULT", "COVERAGE_SHARD_RESULT",
                       "PATCH_COVERAGE_RESULT",
                       "IGNORED_DB_STORES_RESULT", "IGNORED_DB_RUNTIME_RESULT",
                       "FUZZ_COMPILE_RESULT", "NEXTEST_RESULT"):
            self.assertIn(marker, body, marker)

    def test_legacy_duplicate_aggregate_is_removed(self):
        self.assertNotIn("required-checks", self.workflows["check.yml"]["jobs"])

    def test_advisory_container_smoke_has_an_exemption_reason(self):
        source = (ROOT / ".github/workflows/check.yml").read_text()
        block = re.search(r"(?ms)^  container:\n(.*?)(?=^  [a-z][a-z-]*:|\Z)", source)[1]
        self.assertRegex(block, r"(?m)^    # ci-ok: exempt \S.+")

    def test_fuzz_compile_is_required_with_bounded_seed_replay(self):
        workflow = self.workflows["check.yml"]
        job = workflow["jobs"]["fuzz-compile"]
        self.assertEqual(job.get("name"), "fuzz compile")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        self.assertIn("job-inputs", job.get("needs", []))
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job.get("if", ""))
        self.assertIn("fuzz-compile", job.get("runs-on", ""))
        text = str(job)
        install = [step for step in job.get("steps", [])
                   if step.get("name") == "Install pinned cargo-fuzz"]
        self.assertEqual(len(install), 1)
        self.assertEqual(install[0].get("run"), "cargo install cargo-fuzz --version 0.13.2 --locked")
        self.assertIn("nightly-2026-10-01", text)
        self.assertNotIn("max_total_time", text)

        # Separate workspace: compilation and the bounded replay are distinct steps in fuzz/.
        build = [step for step in job.get("steps", []) if "fuzz build" in str(step.get("run", ""))]
        self.assertEqual(len(build), 1)
        self.assertEqual(build[0].get("working-directory"), "fuzz")
        self.assertEqual(build[0].get("run"), "cargo +nightly-2026-10-01 fuzz build")
        targets = ("eventsub_delivery", "helix_token", "custom_command_template", "fb_page_events",
                   "database_url", "feeds_http", "self_role_custom_id", "helix_subscription_listing")
        fuzz_runs = [step for step in job.get("steps", [])
                     if "fuzz run" in step.get("run", "")]
        self.assertEqual(len(fuzz_runs), len(targets))
        for target, replay in zip(targets, fuzz_runs):
            with self.subTest(target=target):
                self.assertEqual(replay.get("working-directory"), "fuzz")
                self.assertIsNot(replay, build[0])
                self.assertEqual(
                    replay.get("run"),
                    f"cargo +nightly-2026-10-01 fuzz run {target} -- -runs=0 -timeout=10 -rss_limit_mb=2048",
                )

                # Each bounded replay executes its committed target and seeds.
                self.assertTrue((ROOT / f"fuzz/fuzz_targets/{target}.rs").is_file())
                corpus = ROOT / "fuzz/corpus" / target
                self.assertTrue(corpus.is_dir())
                self.assertTrue(any(seed.is_file() for seed in corpus.iterdir()))

        # Pinned dependencies: the committed fuzz/Cargo.lock is the gate's
        # version inventory. `--locked` fails the required job instead of
        # silently resolving fresh crates.io versions on every run.
        fetch = [step for step in job.get("steps", []) if "fetch --locked" in str(step.get("run", ""))]
        self.assertEqual(len(fetch), 1)
        self.assertEqual(fetch[0].get("working-directory"), "fuzz")
        self.assertEqual(fetch[0].get("run"), "cargo +nightly-2026-10-01 fetch --locked")
        agg = workflow["jobs"]["ci-ok"]
        self.assertIn("fuzz-compile", agg["needs"])
        self.assertIn("FUZZ_COMPILE_RESULT", "\n".join(step.get("run", "") for step in agg["steps"]))

    def test_ignored_db_runtime_routes_claimed_ignored_tests(self):
        # These two filters are the only required-lane coverage for eleven
        # ignored tests. Cargo exits 0 when a filter matches nothing, so pin
        # partition presence, the exact argv (a one-character typo would
        # silently unroute the tests) and the frontier's guarded URL. The
        # lane runs partitioned: the "runtime" partition of
        # run-test-partition.py carries these filters, the
        # ignored-db-runtime-partition workers execute them, and the
        # ignored-db-runtime aggregate verifies every partition receipt.
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "run_test_partition", ROOT / "scripts/ci/run-test-partition.py")
        partitions = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(partitions)
        self.assertIn("runtime", partitions.PARTITIONS["ignored-db-runtime"])
        commands = partitions.runtime_commands("runtime")
        evidence = [entry for entry in commands
                    if "containment_runtime_evidence_" in entry[0]]
        self.assertEqual(len(evidence), 1)
        self.assertEqual(
            ["cargo"] + evidence[0][0],
            ["cargo", "test", "-p", "two-bot", "--locked", "--lib", "--bins",
             "containment_runtime_evidence_", "--", "--include-ignored"],
        )
        frontier = [entry for entry in commands
                    if "configured_database_reports_seeded_family_frontier" in entry[0]]
        self.assertEqual(len(frontier), 1)
        self.assertEqual(
            frontier[0][1],
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        )
        self.assertEqual(
            ["cargo"] + frontier[0][0],
            ["cargo", "test", "-p", "two-bot", "--locked", "--lib", "--bins",
             "configured_database_reports_seeded_family_frontier",
             "--", "--include-ignored"],
        )
        # The partition worker executes the script for this lane, and the
        # aggregate verifies the receipts; a filter the worker never runs
        # is unrouted even when the script still lists it.
        worker = self.workflows["check.yml"]["jobs"]["ignored-db-runtime-partition"]
        run_steps = [step for step in worker.get("steps", [])
                     if "run-test-partition.py" in str(step.get("run", ""))
                     and '--lane "$CI_TEST_LANE"' in str(step.get("run", ""))]
        self.assertEqual(len(run_steps), 1)
        self.assertEqual(
            run_steps[0].get("env", {}).get("CI_TEST_LANE"), "ignored-db-runtime")
        aggregate = self.workflows["check.yml"]["jobs"]["ignored-db-runtime"]
        self.assertIn("ignored-db-runtime-partition", aggregate.get("needs", []))
        verify = [step for step in aggregate.get("steps", [])
                  if "--verify-receipts" in str(step.get("run", ""))]
        self.assertEqual(len(verify), 1)
        self.assertEqual(
            verify[0].get("env", {}).get("CI_TEST_LANE"), "ignored-db-runtime")

        # Each filter substring must still match its ignored test fns in the
        # source tree: a renamed test would otherwise keep every gate green
        # while the required lane runs an empty filter.
        sources = [path.read_text(encoding="utf-8", errors="replace")
                   for path in (ROOT / "crates").rglob("*.rs")]
        evidence_fns = [name for body in sources
                        for name in re.findall(r"fn (containment_runtime_evidence_\w+)", body)]
        self.assertEqual(len(evidence_fns), 10, sorted(set(evidence_fns)))
        frontier_fns = [name for body in sources
                        for name in re.findall(r"fn (configured_database_reports_seeded_family_frontier)\b", body)]
        self.assertEqual(frontier_fns, ["configured_database_reports_seeded_family_frontier"])

    def test_nextest_is_required_and_rust_gated(self):
        job = self.workflows["check.yml"]["jobs"]["nextest"]
        self.assertEqual(job.get("name"), "rust nextest")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        self.assertIn("job-inputs", job.get("needs", []))
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job.get("if", ""))
        self.assertEqual(job.get("uses"), "./.github/workflows/reusable-rust-nextest.yml")
        self.assertNotIn("schedule", self.workflows["check.yml"].get("on", {}).get("pull_request", {}))
        agg = self.workflows["check.yml"]["jobs"]["ci-ok"]
        self.assertIn("nextest", agg["needs"])
        self.assertIn("NEXTEST_RESULT", "\n".join(step.get("run", "") for step in agg["steps"]))

    def aggregator(self, name):
        job = self.workflows["check.yml"]["jobs"][name]
        step = next(step for step in job["steps"] if "python3 -" in step.get("run", ""))
        script = textwrap.dedent(step["run"].split("<<'PY'\n", 1)[1].rsplit("PY", 1)[0])
        return job, step["env"], script

    def test_every_job_is_aggregated_or_explicitly_exempt(self):
        # A job missing from ci-ok is a false gate: its failure would never
        # turn the required verdict red.
        jobs = set(self.workflows["check.yml"]["jobs"])
        for name in AGGREGATORS:
            job, env, script = self.aggregator(name)
            with self.subTest(aggregator=name):
                missing = jobs - set(job["needs"]) - NOT_AGGREGATED
                self.assertEqual(missing, set(), f"{name} does not need {sorted(missing)}")
                for needed in set(job["needs"]):
                    consumed = [key for key, value in env.items()
                                if value == "${{ needs.%s.result }}" % needed]
                    self.assertEqual(len(consumed), 1, f"{name}: {needed} result is not mapped into the gate")
                    self.assertIn(consumed[0], script, f"{name}: {needed} result is mapped but never read")

    def run_aggregator(self, name, results, selected="true"):
        _, env, script = self.aggregator(name)
        environ = {key: "" for key in env}
        for key, value in env.items():
            needed = re.search(r"needs\.([a-z-]+)\.result", value)
            if needed:
                environ[key] = results.get(needed[1], "success")
            elif value.startswith("${{ needs.job-inputs.outputs."):
                environ[key] = selected
        environ["JOB_INPUTS_RESULT"] = results.get("job-inputs", "success")
        run = subprocess.run([sys.executable, "-c", script], env={**os.environ, **environ},
                             capture_output=True, text=True)
        return run.returncode

    def test_aggregators_reject_a_failed_cancelled_or_unexpectedly_skipped_job(self):
        needed = set(self.workflows["check.yml"]["jobs"]["ci-ok"]["needs"]) - {"job-inputs"}
        for name in AGGREGATORS:
            self.assertEqual(self.run_aggregator(name, {}), 0, f"{name}: all green")
            for job in sorted(needed):
                for result in ("failure", "cancelled", "skipped"):
                    with self.subTest(aggregator=name, job=job, result=result):
                        self.assertEqual(self.run_aggregator(name, {job: result}, selected="true"), 1)
            with self.subTest(aggregator=name, job="job-inputs"):
                self.assertEqual(self.run_aggregator(name, {"job-inputs": "failure"}), 1)

    def test_container_inputs_is_an_always_run_gate_even_on_docs_only_changes(self):
        selector = self.workflows["check.yml"]["jobs"]["container-inputs"]
        self.assertNotIn("if", selector)
        for selected in ("true", "false"):
            for result in ("failure", "cancelled", "skipped", "", "pending", "queued", "in_progress"):
                with self.subTest(selected=selected, result=result):
                    self.assertEqual(
                        self.run_aggregator("ci-ok", {"container-inputs": result}, selected=selected), 1
                    )

    def test_aggregators_accept_a_skip_only_when_the_selector_deselected_the_job(self):
        # Docs-only PRs: selector-gated jobs skip, always-run selectors and
        # lint still pass. Docs changes continue to select the worker lane.
        gated = {"rust-tests", "test-partition-plan", "rust-tests-partition",
                 "ignored-db-runtime-partition",
                 "coverage-plan", "coverage-shard", "patch-coverage",
                 "ignored-db-stores", "ignored-db-runtime", "self-role-store",
                 "community-db", "feeds-db", "tickets-postgres", "moderation-db",
                 "parity-docs", "supply-chain", "fuzz-compile", "nextest"}
        for name in AGGREGATORS:
            with self.subTest(aggregator=name):
                skipped = {job: "skipped" for job in gated}
                self.assertEqual(self.run_aggregator(name, skipped, selected="false"), 0)
                for always_run in ("check", "container-inputs"):
                    self.assertEqual(
                        self.run_aggregator(name, {**skipped, always_run: "skipped"}, selected="false"), 1,
                        f"{always_run} is an always-run gate and may never skip",
                    )

    def test_gitleaks_runs_whatever_pr_lint_concluded(self):
        self.assertIn("!cancelled()", self.workflows["supply-chain.yml"]["jobs"]["gitleaks"]["if"])


if __name__ == "__main__":
    unittest.main()

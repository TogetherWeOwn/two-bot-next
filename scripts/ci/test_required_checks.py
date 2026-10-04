"""Offline pins for the required-check contract CONTRIBUTING states.

GitHub leaves a required check pending forever when the workflow that owns it
never starts (a `paths`/`branches` filter on the trigger), and counts a skipped
job as passing. So each required context must be exactly one job, sit in a
workflow whose pull_request trigger is unfiltered, and the `check` aggregator
must still run, and fail, when job selection or its dependencies fail.
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
REQUIRED = {"check", "worker check", "gitleaks", "pr-lint"}
TRIGGER_FILTERS = {"paths", "paths-ignore", "branches", "branches-ignore"}
# Jobs in check.yml that deliberately sit outside the aggregators: `job-inputs`
# is the selector the aggregators read, the two container jobs are the image
# smoke (not a correctness gate, up to 30 minutes), and the aggregators
# themselves.
NOT_AGGREGATED = {"job-inputs", "container-inputs", "container", "required-checks", "ci-ok"}
AGGREGATORS = ("required-checks", "ci-ok")


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

    def test_aggregator_runs_and_fails_when_job_selection_fails(self):
        check = self.workflows["check.yml"]["jobs"]["check"]
        self.assertIn("always()", check["if"])
        self.assertIn("job-inputs", check["needs"])
        guard = next(step for step in check["steps"]
                     if step.get("name") == "require job inputs selection to pass")
        self.assertEqual(guard["if"], "needs.job-inputs.result != 'success'")
        self.assertEqual(guard["run"], "exit 1")

    def test_required_checks_aggregator_covers_path_filtered_jobs(self):
        agg = self.workflows["check.yml"]["jobs"]["required-checks"]
        self.assertEqual(agg["name"], "required checks")
        self.assertIn("always()", agg["if"])
        for job in ("job-inputs", "supply-chain", "check", "rust-tests",
                    "ignored-db-stores", "ignored-db-runtime", "worker",
                    "parity-docs", "self-role-store", "community-db",
                    "feeds-db", "tickets-postgres", "moderation-db"):
            self.assertIn(job, agg["needs"], job)
        # The container image smoke is not a correctness gate; keeping it out
        # of `needs` keeps this signal off the image-build critical path.
        self.assertNotIn("container", agg["needs"])
        self.assertNotIn("container-inputs", agg["needs"])
        self.assertIn("required-checks", agg["runs-on"])
        body = "\n".join(step.get("run", "") for step in agg["steps"])
        for marker in ("JOB_INPUTS_RESULT", "SUPPLY_CHAIN_RESULT",
                       "CHECK_RESULT", "WORKER_RESULT", "PARITY_DOCS_RESULT",
                       "SELF_ROLE_RESULT", "COMMUNITY_DB_RESULT",
                       "FEEDS_DB_RESULT", "TICKETS_RESULT",
                       "MODERATION_DB_RESULT", "RUST_TESTS_RESULT",
                       "IGNORED_DB_STORES_RESULT", "IGNORED_DB_RUNTIME_RESULT"):
            self.assertIn(marker, body, marker)

    def aggregator(self, name):
        job = self.workflows["check.yml"]["jobs"][name]
        step = next(step for step in job["steps"] if "python3 -" in step.get("run", ""))
        script = textwrap.dedent(step["run"].split("<<'PY'\n", 1)[1].rsplit("PY", 1)[0])
        return job, step["env"], script

    def test_every_job_is_aggregated_or_explicitly_exempt(self):
        # A job added to check.yml but missing from the aggregators is a false
        # gate: its failure would never turn the required `ci-ok` red.
        jobs = set(self.workflows["check.yml"]["jobs"])
        for name in AGGREGATORS:
            job, env, script = self.aggregator(name)
            with self.subTest(aggregator=name):
                missing = jobs - set(job["needs"]) - NOT_AGGREGATED
                self.assertEqual(missing, set(), f"{name} does not need {sorted(missing)}")
                for needed in set(job["needs"]) - {"job-inputs"}:
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

    def test_aggregators_accept_a_skip_only_when_the_selector_deselected_the_job(self):
        # Docs-only PRs: the selector reports rust/worker/parity/supply = false,
        # the selector-gated jobs skip and the always-run ones still pass.
        gated = {"rust-tests", "ignored-db-stores", "ignored-db-runtime", "self-role-store",
                 "community-db", "feeds-db", "tickets-postgres", "moderation-db",
                 "parity-docs", "supply-chain"}
        for name in AGGREGATORS:
            with self.subTest(aggregator=name):
                skipped = {job: "skipped" for job in gated}
                self.assertEqual(self.run_aggregator(name, skipped, selected="false"), 0)
                self.assertEqual(self.run_aggregator(name, {**skipped, "check": "skipped"}, selected="false"), 1,
                                 "the lint lane is an always-run gate and may never skip")

    def test_gitleaks_runs_whatever_pr_lint_concluded(self):
        self.assertIn("!cancelled()", self.workflows["supply-chain.yml"]["jobs"]["gitleaks"]["if"])


if __name__ == "__main__":
    unittest.main()

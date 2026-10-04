"""Offline pins for the required-check contract CONTRIBUTING states.

GitHub leaves a required check pending forever when the workflow that owns it
never starts (a `paths`/`branches` filter on the trigger), and counts a skipped
job as passing. So each required context must be exactly one job, sit in a
workflow whose pull_request trigger is unfiltered, and the `check` aggregator
must still run, and fail, when job selection or its dependencies fail.
"""

from pathlib import Path
import re
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
REQUIRED = {"check", "worker check", "gitleaks", "pr-lint"}
TRIGGER_FILTERS = {"paths", "paths-ignore", "branches", "branches-ignore"}


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
        for job in ("job-inputs", "supply-chain", "check", "worker",
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
                       "MODERATION_DB_RESULT"):
            self.assertIn(marker, body, marker)

    def test_gitleaks_runs_whatever_pr_lint_concluded(self):
        self.assertIn("!cancelled()", self.workflows["supply-chain.yml"]["jobs"]["gitleaks"]["if"])


if __name__ == "__main__":
    unittest.main()

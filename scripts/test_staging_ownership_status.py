"""Offline pins for the status-only staging ownership workflow; stdlib only."""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/staging-ownership-status.yml"
WRAPPER = ROOT / "wrangler/scripts/staging-ownership-status.mjs"
CHECK = ROOT / ".github/workflows/check.yml"


def strip_comments(text):
    return "\n".join(
        line.split("#", 1)[0] for line in text.splitlines()
    )


class WorkflowSurfaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.code = strip_comments(cls.text)

    def test_workflow_exists_and_is_manual_only(self):
        self.assertIn("name: staging-ownership-status", self.text)
        self.assertIn("workflow_dispatch:", self.text)
        for trigger in ("push:", "pull_request:", "schedule:", "workflow_call:", "workflow_run:"):
            self.assertNotIn(trigger, self.code)

    def test_no_arbitrary_inputs_or_ref_target(self):
        self.assertNotIn("inputs.", self.code)
        self.assertNotIn("github.sha", self.code)
        self.assertNotIn("github.ref_name", self.code)
        self.assertNotIn("TARGET_VERSION", self.code)
        self.assertNotIn("STAGING_WORKER_URL:", self.code.replace("STAGING_WORKER_URL: ${{ vars.STAGING_WORKER_URL }}", ""))

    def test_staging_environment_boundary_only(self):
        self.assertIn("environment: staging", self.text)
        self.assertIn("STAGING_WORKER_URL: ${{ vars.STAGING_WORKER_URL }}", self.text)
        self.assertIn("OWNERSHIP_CONTROL_TOKEN: ${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}", self.text)
        for forbidden in ("PRODUCTION_", "CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID",
                          "TWO_BOT_STAGING_PLAN_DATABASE_URL", "METRICS_SCRAPE_TOKEN"):
            self.assertNotIn(forbidden, self.text)

    def test_main_only_guard_and_timeouts(self):
        self.assertIn("if: github.ref == 'refs/heads/main'", self.text)
        self.assertIn("timeout-minutes: 10", self.text)
        self.assertIn("persist-credentials: false", self.text)
        self.assertIn("concurrency:\n  group: staging-ownership-status", self.text)

    def test_runner_routing_and_pinned_actions(self):
        self.assertIn("contains(fromJSON(vars.CI_OVERFLOW_JOBS || '[]'), 'status')", self.text)
        self.assertIn("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1", self.text)
        self.assertIn("actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1", self.text)

    def test_get_only_no_mutation(self):
        self.assertIn("node scripts/staging-ownership-status.mjs", self.text)
        self.assertIn("node scripts/ownership-control.mjs preflight", self.text)
        for forbidden in ("deployment-takeover", "wrangler deploy", "staging_rollback_drill",
                          "staging_container_drill", "worker_container_rollback"):
            self.assertNotIn(forbidden, self.code)
        # No POST, takeover, fence or transition verbs in executable lines.
        for verb in (re.compile(r"\bPOST\b"), re.compile(r"\btakeover\b", re.I),
                     re.compile(r"\bfence\b", re.I), re.compile(r"\btransition\b", re.I)):
            for line in self.code.splitlines():
                stripped = line.strip()
                if not stripped or stripped.startswith("#"):
                    continue
                # Allow the header comment block's explanatory "no takeover/fence" line.
                if stripped.startswith("#"):
                    continue
                if verb.search(stripped) and "read-only" not in stripped.lower():
                    # The workflow header explains the negative scope; run/env lines must not carry verbs.
                    if stripped.startswith(("name:", "run:", "env:", "- name:")):
                        self.assertNotRegex(stripped, verb, f"mutating verb in: {stripped}")

    def test_no_shell_tracing_or_secret_echo(self):
        self.assertNotIn("set -x", self.code)
        self.assertNotIn("set -o xtrace", self.code)
        self.assertNotIn("::add-mask::", self.code)
        self.assertNotIn("echo $OWNERSHIP", self.code)
        self.assertNotIn("echo $STAGING", self.code)
        self.assertNotIn("printenv", self.code)
        self.assertNotIn("env |", self.code)

    def test_strict_allowlist_output_validation(self):
        self.assertIn('"owner_id"', self.text)
        self.assertIn('"observed_at"', self.text)
        self.assertIn('set(d)=={"owner_id","observed_at"}', self.text)
        self.assertNotIn("actor", self.code.replace("OWNERSHIP_ACTOR", ""))
        self.assertNotIn("running", self.code)

    def test_wrapper_reuses_status_action(self):
        source = WRAPPER.read_text()
        self.assertIn("action: 'status'", source)
        self.assertNotIn("deployment-takeover", source)
        self.assertNotIn("takeover", source.replace("Takeover does", ""))
        self.assertIn("STAGING_WORKER_URL", source)
        self.assertIn("OWNERSHIP_CONTROL_TOKEN", source)


def worker_steps(source):
    worker = source.split("  worker:\n", 1)[1]
    return re.split(r"(?m)^      - ", worker)[1:]


def assert_pinned(steps, needle, case):
    matching = [step for step in steps if needle in step]
    case.assertEqual(len(matching), 1, needle)
    case.assertNotRegex(matching[0], r"(?m)^\s*(?:if|continue-on-error):")
    case.assertNotIn("|| true", matching[0])


class CheckWiringTests(unittest.TestCase):
    def test_required_worker_ci_runs_this_offline_suite(self):
        steps = worker_steps(CHECK.read_text())
        for needle in ("scripts/test-staging-ownership-status.mjs",
                       "test_staging_ownership_status.py"):
            assert_pinned(steps, needle, self)

    def test_required_worker_ci_runs_the_production_status_suite(self):
        # The production ownership-status retry suite lives beside the
        # staging suites in the worker job; deleting its step (or gating
        # it) must fail here instead of silently dropping the coverage.
        steps = worker_steps(CHECK.read_text())
        assert_pinned(steps, "scripts/test-production-ownership-control.mjs",
                      self)


if __name__ == "__main__":
    unittest.main()

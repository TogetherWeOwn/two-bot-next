"""Offline checks for guarded channel suites and self-hosted nightly isolation."""

import os
import re
import subprocess
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def step(workflow, name):
    marker = f"      - name: {name}\n"
    return workflow.split(marker, 1)[1].split("      - name:", 1)[0]


class ChannelCiTests(unittest.TestCase):
    def setUp(self):
        self.workflow = (ROOT / ".github/workflows/nightly.yml").read_text()

    def assert_isolated(self, workflow):
        # Public-repo routing (TOG-12339): the shared fromJSON expression
        # selects ubuntu-latest for public repos, so the literal appears in
        # the expression. What isolation forbids is a hardcoded runner.
        self.assertNotIn("runs-on: ubuntu-latest", workflow)
        self.assertIn("!github.event.repository.private", workflow)
        self.assertNotIn("        ports:", workflow)
        self.assertIn("    container:\n      image: rust:bookworm@sha256:", workflow)
        prepare = step(workflow, "Prepare isolated test databases")
        self.assertNotIn("docker exec", prepare)
        self.assertNotIn("/etc/hosts", prepare)
        self.assertIn("createdb -h agent-testdb -U agent_test", prepare)
        # The loopback forward lives in the prerequisites step since main
        # split DB bootstrap; its presence in the workflow proves the
        # job-private service network without published host ports.
        self.assertIn("TCP:agent-testdb:5432", workflow)

    def assert_guarded_suites(self, workflow):
        broad = step(workflow, "Full workspace sweep including ignored tests")
        self.assertIn("--skip channel_moderation_store::", broad)
        source = (ROOT / "crates/discord/tests/channel_moderation_runtime.rs").read_text()
        names = re.findall(r"#\[ignore[^\n]*\]\s*async fn (\w+)", source)
        self.assertTrue(names, "runtime ignored tests must be discovered")
        for name in names:
            self.assertIn(f"--skip {name}\n", broad)
        for name, target in [
            ("Channel moderation ignored tests with their guarded URL", "channel_moderation_store::"),
            ("Channel moderation runtime acceptance with its guarded URL", "--test channel_moderation_runtime"),
        ]:
            guarded = step(workflow, name)
            self.assertIn("postgres://agent_test@agent-testdb:5432/agent_test", guarded)
            self.assertIn(target, guarded)
            self.assertIn("--include-ignored", guarded)
            self.assertNotIn("--skip", guarded)

    def selected_jobs(self, event, result="", exit_code="0", base="base", head="head", diff="", diff_fails=False):
        selector = step(self.workflow, "Select affected jobs or run the full nightly suite")
        script = textwrap.dedent(re.split(r"\n  (?=\S)", selector.split("        run: |\n", 1)[1], maxsplit=1)[0])
        mock = ('python3() { printf "%s\\n" "$SELECTOR_RESULT"; return "$SELECTOR_EXIT"; }\n'
                'git() { [ -z "$GIT_FAILS" ] || return 128; printf "%s\\0" $CHANGED_FILES; }\n')
        env = dict(os.environ, EVENT_NAME=event, BASE_SHA=base, HEAD_SHA=head, CHANGED_FILES=diff,
                   GIT_FAILS="1" if diff_fails else "",
                   SELECTOR_RESULT=result, SELECTOR_EXIT=exit_code, GITHUB_OUTPUT="/dev/stdout")
        run = subprocess.run(["bash", "-c", mock + script], env=env,
                             check=True, capture_output=True, text=True)
        return dict(line.split("=", 1) for line in run.stdout.splitlines())

    def gate(self, job):
        body = re.split(r"\n  (?=\S)", self.workflow.split(f"\n  {job}:\n", 1)[1], maxsplit=1)[0]
        self.assertIn("    needs: changes\n", body)
        return re.search(r"^    if: \$\{\{ (.+) \}\}$", body, re.M).group(1)

    def runs(self, gate, output, selector_failed=False, cancelled=False):
        """Evaluate a nightly gate the way Actions does for a `changes` outcome.

        Without `!cancelled()` Actions prepends an implicit `success()` that
        skips the job whenever the selector job failed or timed out.
        """
        if cancelled:
            return False
        if "!cancelled()" not in gate and selector_failed:
            return False
        value = "" if selector_failed else output
        return gate.endswith(" != 'false'") and value != "false"

    def test_nightly_gates_heavy_jobs_at_job_level(self):
        self.assertNotIn("    paths:", self.workflow)
        for job, output in [("pipeline-benchmark", "rust"), ("sweep", "rust"), ("advisories", "supply")]:
            self.assertEqual(self.gate(job), f"!cancelled() && needs.changes.outputs.{output} != 'false'")

    def test_nightly_gates_fail_closed_when_the_selector_job_fails(self):
        for job in ["pipeline-benchmark", "sweep", "advisories"]:
            gate = self.gate(job)
            self.assertTrue(self.runs(gate, "true"), job)
            self.assertFalse(self.runs(gate, "false"), job)
            # Selector failure or timeout: no output, the heavy job must still run.
            self.assertTrue(self.runs(gate, "", selector_failed=True), job)
            # A cancelled run (superseded push) must not start heavy jobs.
            self.assertFalse(self.runs(gate, "true", cancelled=True), job)
        # The pre-fix gate would have skipped the job on selector failure.
        old = "needs.changes.outputs.rust != 'false'"
        self.assertFalse(self.runs(old, "", selector_failed=True))

    def test_nightly_skips_pull_requests_unless_the_nightly_wiring_changes(self):
        # The nightly suite runs on its schedule; the required `check` matrix covers
        # affected suites per PR. A Rust or dependency change alone no longer runs it.
        skip = {"rust": "false", "supply": "false"}
        full = {"rust": "true", "supply": "true"}
        self.assertEqual(self.selected_jobs("pull_request", "rust=true\nsupply=true",
                                            diff="crates/bot/src/lib.rs Cargo.lock"), skip)
        for wiring in [".github/workflows/nightly.yml", ".github/workflows/pipeline-benchmark.yml",
                       "scripts/job-inputs.py"]:
            self.assertEqual(self.selected_jobs("pull_request", diff=f"README.md {wiring}"), full, wiring)
        # A path that merely contains a wiring name does not count.
        self.assertEqual(self.selected_jobs("pull_request", diff="docs/.github/workflows/nightly.yml.md"), skip)
        # A failing diff never reports an empty successful change set: it runs the full suite.
        self.assertEqual(self.selected_jobs("pull_request", diff_fails=True), full)

    def test_nightly_selection_defaults_to_full_coverage(self):
        full = {"rust": "true", "supply": "true"}
        for event in ["schedule", "workflow_dispatch", "push"]:
            self.assertEqual(self.selected_jobs(event, "rust=false\nsupply=false"), full)
        self.assertEqual(self.selected_jobs("pull_request", base=""), full)
        self.assertEqual(self.selected_jobs("pull_request", head=""), full)

    def test_nightly_uses_job_private_service_network(self):
        self.assert_isolated(self.workflow)

    def test_all_channel_tests_run_with_their_guarded_database(self):
        self.assert_guarded_suites(self.workflow)

    def test_published_host_port_is_rejected(self):
        with self.assertRaises(AssertionError):
            self.assert_isolated(self.workflow + "\n        ports:\n          - 5432:5432\n")

    def test_missing_runtime_route_is_rejected(self):
        with self.assertRaises(AssertionError):
            self.assert_guarded_suites(self.workflow.replace("--test channel_moderation_runtime", "--test wrong_suite"))


if __name__ == "__main__":
    unittest.main()

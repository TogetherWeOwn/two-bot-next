"""Offline checks for guarded channel suites and self-hosted nightly isolation."""

import re
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

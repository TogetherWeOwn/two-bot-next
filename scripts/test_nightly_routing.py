"""Keep guarded fixtures, mock deadlines and strict docs correctly separated."""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/nightly.yml"


class NightlyRoutingTests(unittest.TestCase):
    def test_every_ignored_channel_fixture_is_routed_out_of_broad_bootstrap(self):
        source = (ROOT / "crates/core/src/channel_moderation_store.rs").read_text()
        names = set(re.findall(r'#\[ignore[^\]]*\]\s*async fn (\w+)\(', source))
        self.assertTrue(names, "guarded ignored fixtures must be discovered")
        workflow = WORKFLOW.read_text()
        broad = workflow.split("- name: Full workspace sweep including ignored tests", 1)[1]
        broad = broad.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        routed = set(re.findall(r"--skip channel_moderation_store::tests::(\w+)", broad))
        self.assertEqual(routed, names, "no guarded fixture runs with the incompatible bootstrap")

    def test_dedicated_channel_suite_runs_all_fixtures_with_unchanged_guard(self):
        workflow = WORKFLOW.read_text()
        dedicated = workflow.split("- name: Channel moderation ignored tests with their guarded URL", 1)[1]
        dedicated = dedicated.split("- name: Feed store ignored tests with their guarded URL", 1)[0]
        self.assertIn("steps.databases.outcome == 'success'", dedicated)
        self.assertIn("TWO_TEST_DATABASE_URL: postgres://agent_test@agent-testdb:5432/agent_test", dedicated)
        self.assertIn("cargo test -p two-bot-core --all-features --lib --locked channel_moderation_store::", dedicated)
        self.assertIn("-- --include-ignored --test-threads=1", dedicated)
        self.assertNotIn("--skip", dedicated, "routing is not permanent exclusion")

    def test_response_classification_fixture_does_not_shorten_default_deadline(self):
        source = (ROOT / "crates/discord/src/internal_actions/tests.rs").read_text()
        fixture = source.split("    fn executor(", 1)[1].split("\n    fn ", 1)[0]
        self.assertNotIn("executor.timeout =", fixture)

    def test_short_deadlines_remain_explicit_in_timeout_coverage(self):
        source = (ROOT / "crates/discord/src/internal_actions/tests.rs").read_text()
        self.assertTrue("    fn deadline_executor(" in source, "deadline fixtures must be explicit")
        for name in [
            "timeout_and_lost_response_are_unknown_and_not_retried",
            "deadline_covers_success_body_and_truncated_body_is_unknown",
        ]:
            body = source.split(f"async fn {name}()", 1)[1].split("#[tokio::test]", 1)[0]
            self.assertIn("mock.deadline_executor(keys())", body)
            self.assertIn("UnknownReason::Timeout", body)
            self.assertIn("assert_eq!(mock.count(), 1)", body)

    def test_broad_sweep_binds_member_testdb_for_member_ledger_fixtures(self):
        workflow = WORKFLOW.read_text()
        broad = workflow.split("- name: Full workspace sweep including ignored tests", 1)[1]
        broad = broad.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        self.assertIn("MEMBER_TESTDB: agent-testdb", broad)

    def test_discord_reference_urls_are_hyperlinks_without_suppressing_doc_lint(self):
        for name in ["executor/tickets.rs", "internal_exec/member.rs"]:
            source = (ROOT / "crates/discord/src" / name).read_text()
            bare = [
                line for line in source.splitlines()
                if re.search(r"^\s*//[/!] ", line) and re.search(r"(?<!<)https?://", line)
            ]
            self.assertEqual(bare, [], f"{name} must wrap doc URLs in <angle brackets>")

    def test_cutover_reference_urls_are_hyperlinks_without_suppressing_doc_lint(self):
        for name in ["self_role_store.rs", "tickets.rs"]:
            source = (ROOT / "crates/cutover/src" / name).read_text()
            self.assertEqual(re.findall(r"(?m)^//! https?://[^\n]+", source), [])
            self.assertEqual(len(re.findall(r"(?m)^//! <https?://[^>]+>$", source)), 2)
        workflow = WORKFLOW.read_text()
        self.assertIn("RUSTDOCFLAGS: -D warnings", workflow)
        self.assertIn("cargo doc --workspace --all-features --no-deps --locked", workflow)


if __name__ == "__main__":
    unittest.main()

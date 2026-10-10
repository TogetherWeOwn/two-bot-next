"""Keep guarded fixtures, mock deadlines and strict docs correctly separated."""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/nightly.yml"
CHECK = ROOT / ".github/workflows/check.yml"


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
        dedicated = dedicated.split("- name: Discord channel-moderation executor ignored tests", 1)[0]
        self.assertIn("steps.databases.outcome == 'success'", dedicated)
        self.assertIn("TWO_TEST_DATABASE_URL: postgres://agent_test@agent-testdb:5432/agent_test", dedicated)
        self.assertIn("cargo test -p two-bot-core --all-features --lib --locked channel_moderation_store::", dedicated)
        self.assertIn("-- --include-ignored --test-threads=1", dedicated)
        self.assertNotIn("--skip", dedicated, "routing is not permanent exclusion")

    def test_every_ignored_discord_executor_fixture_is_routed_out_of_broad_bootstrap(self):
        source = (ROOT / "crates/discord/tests/internal_channel_moderation.rs").read_text()
        names = set(re.findall(r'#\[ignore[^\]]*\]\s*async fn (\w+)\(', source))
        self.assertTrue(names, "guarded ignored fixtures must be discovered")
        workflow = WORKFLOW.read_text()
        broad = workflow.split("- name: Full workspace sweep including ignored tests", 1)[1]
        broad = broad.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        routed = set(re.findall(r"--skip (\w+)", broad))
        for name in names:
            self.assertIn(name, routed, f"{name} must skip the incompatible bootstrap")

    def test_dedicated_discord_executor_suite_runs_all_fixtures_with_unchanged_guard(self):
        workflow = WORKFLOW.read_text()
        dedicated = workflow.split("- name: Discord channel-moderation executor ignored tests", 1)[1]
        dedicated = dedicated.split("- name: Feed store ignored tests with their guarded URL", 1)[0]
        self.assertIn("steps.databases.outcome == 'success'", dedicated)
        self.assertIn("TWO_TEST_DATABASE_URL: postgres://agent_test@agent-testdb:5432/agent_test", dedicated)
        self.assertIn("cargo test -p two-bot-discord --all-features --test internal_channel_moderation --locked", dedicated)
        self.assertIn("-- --include-ignored --test-threads=1", dedicated)
        self.assertNotIn("--skip", dedicated, "routing is not permanent exclusion")

    def test_response_classification_fixture_does_not_shorten_default_deadline(self):
        source = (ROOT / "crates/discord/src/internal_actions/tests.rs").read_text()
        fixture = source.split("    fn executor(", 1)[1].split("\n    fn ", 1)[0]
        # Frozen-clock contract (#151): the fixture executor pins the short
        # 100ms deadline by default so timeout tests never use production timing.
        self.assertIn("executor.timeout = Duration::from_millis(100)", fixture)
        self.assertIn("executor.response_received =", fixture)

    def test_short_deadlines_remain_explicit_in_timeout_coverage(self):
        source = (ROOT / "crates/discord/src/internal_actions/tests.rs").read_text()
        # Frozen-clock contract (#151): slowness is modeled with stall_* flags
        # plus controlled advance(), not wall-clock delay/body_delay fields.
        self.assertNotIn("    fn deadline_executor(", source)
        self.assertNotIn("body_delay", source)
        for name in [
            "timeout_and_lost_response_are_unknown_and_not_retried",
            "deadline_covers_success_body_and_truncated_body_is_unknown",
        ]:
            body = source.split(f"async fn {name}()", 1)[1].split("#[tokio::test]", 1)[0]
            self.assertIn("run_until_timeout(&mock, &executor,", body)
            self.assertTrue(
                "stall_response = true" in body or "stall_body = true" in body,
                f"{name} must model slowness with a stall_* flag",
            )
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

    def test_broad_sweep_binds_store_test_url_to_the_disposable_service(self):
        source = (ROOT / "crates/store/tests/postgres.rs").read_text()
        self.assertIn('std::env::var("TEST_DATABASE_URL")', source)
        workflow = WORKFLOW.read_text()
        broad = workflow.split("- name: Full workspace sweep including ignored tests", 1)[1]
        broad = broad.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        self.assertIn("TEST_DATABASE_URL: postgresql://agent_test@agent-testdb:5432/postgres", broad)

    def test_every_ignored_raid_list_fixture_is_routed_to_its_guarded_url(self):
        source = (ROOT / "crates/cutover/tests/raid_list_db.rs").read_text()
        names = set(re.findall(r'#\[ignore[^\]]*\]\s*async fn (\w+)\(', source))
        self.assertTrue(names, "guarded ignored fixtures must be discovered")
        workflow = WORKFLOW.read_text()
        broad = workflow.split("- name: Full workspace sweep including ignored tests", 1)[1]
        broad = broad.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        routed = set(re.findall(r"--skip (\w+)", broad))
        for name in names:
            self.assertIn(name, routed, f"{name} must skip the incompatible bootstrap")
        dedicated = workflow.split("- name: Raid list ignored tests with their guarded URL", 1)[1]
        dedicated = dedicated.split("\n      - name: ", 1)[0]
        self.assertIn("steps.databases.outcome == 'success'", dedicated)
        self.assertIn("TWO_TEST_DATABASE_URL: postgres://agent_test:@agent-testdb:5432/agent_test", dedicated)
        self.assertIn("cargo test -p two-bot-cutover --all-features --test raid_list_db --locked", dedicated)
        self.assertIn("-- --include-ignored --test-threads=1", dedicated)
        self.assertNotIn("--skip", dedicated, "routing is not permanent exclusion")

    def test_broad_sweep_scratch_root_exists_before_tests_create_children(self):
        workflow = WORKFLOW.read_text()
        prepare = workflow.split("- name: Prepare isolated test databases", 1)[1]
        prepare = prepare.split("\n      - name: ", 1)[0]
        self.assertIn('"$RUNNER_TEMP/nightly-workspace"', prepare)
        broad = workflow.split("- name: Full workspace sweep including ignored tests", 1)[1]
        broad = broad.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        self.assertIn("PAPERCLIP_RUN_SCRATCH_DIR: ${{ runner.temp }}/nightly-workspace", broad)

    def test_cli_usage_placeholders_are_code_without_suppressing_doc_lint(self):
        source = (ROOT / "crates/cutover/src/bin/audit_switch.rs").read_text()
        docs = [line for line in source.splitlines() if line.startswith("//!")]
        bare = [line for line in docs if re.search(r"<[a-z_]+>", re.sub(r"`[^`]*`", "", line))]
        self.assertEqual(bare, [], "rustdoc reads bare <placeholder> as an unclosed HTML tag")
        self.assertNotIn("invalid_html_tags", source)

    def test_store_pool_doc_link_is_fully_qualified_without_suppressing_doc_lint(self):
        source = (ROOT / "crates/store/src/lib.rs").read_text()
        self.assertIn("[`sqlx::Pool<sqlx::Postgres>`]", source)
        # rustdoc resolves crate-level intra-doc links without the child
        # modules' `use` imports, so a bare [`Pool`] is a broken link
        # under -D warnings even though the type is imported elsewhere.
        bare = re.findall(r"\[`Pool(?:<[^`]*>)?`\]", source)
        self.assertEqual(bare, [], "Pool doc links must be sqlx:: qualified")

    def test_method_doc_links_are_self_qualified_without_suppressing_doc_lint(self):
        source = (ROOT / "crates/cutover/src/rest.rs").read_text()
        self.assertIn("[`Self::scan_channel`]", source)
        # rustdoc resolves intra-doc links at module scope, so a bare
        # [`method`] to a sibling method is a broken link under -D warnings.
        # Free functions (iso_to_millis) stay valid bare links.
        free = set(re.findall(r"(?m)^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn (\w+)\(", source))
        bare = set(re.findall(r"\[`([a-z][a-z0-9_]*)`\]", source))
        self.assertEqual(bare - free, set(), "method links must be Self:: qualified")


    def test_check_runs_exactly_one_full_leveling_runtime(self):
        workflow = CHECK.read_text()
        invocations = re.findall(r"--test leveling_runtime\b", workflow)
        self.assertEqual(len(invocations), 1, "exactly one leveling_runtime invocation must remain")
        self.assertIn("--test leveling_runtime -- --ignored", workflow)
        full = workflow.split("- name: leveling gateway and interaction integration", 1)[1]
        full = full.split("\n      - name: ", 1)[0]
        self.assertIn(
            "TWO_TEST_DATABASE_URL: postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
            full,
        )
        self.assertIn("-- --ignored --test-threads=1", full)

    def test_check_has_no_duplicate_filtered_leveling_invocations(self):
        workflow = CHECK.read_text()
        filtered = re.findall(r"leveling_runtime\s+[A-Za-z_]", workflow)
        self.assertEqual(
            filtered,
            [],
            "filtered leveling_runtime reruns repeat the full ignored run",
        )

    def test_check_has_no_noop_leveling_store_selector_or_obsolete_flag(self):
        workflow = CHECK.read_text()
        self.assertNotIn(
            "leveling_store -- --ignored",
            workflow,
            "leveling_store has no ignored tests; the selector runs zero tests",
        )
        self.assertNotIn("TWO_LEVELING_TEST_CI", workflow)
        self.assertNotIn(
            "TWO_LEVELING_TEST_CI",
            WORKFLOW.read_text(),
            "nightly must not retain the obsolete flag",
        )

    def test_leveling_store_ordinary_tests_stay_in_broad_integration_and_nightly(self):
        source = (ROOT / "crates/core/tests/leveling_store.rs").read_text()
        self.assertEqual(
            len(re.findall(r"#\[tokio::test", source)),
            15,
            "all 15 ordinary store tests must remain",
        )
        self.assertNotIn(
            "#[ignore",
            source,
            "store tests are ordinary; an ignore would silently drop PR coverage",
        )
        check = CHECK.read_text()
        broad = check.split(
            "- name: cargo test (integration, including website acceptance and backup round trip)",
            1,
        )[1].split("\n      - name: ", 1)[0]
        self.assertIn(
            "TWO_TEST_DATABASE_URL: postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
            broad,
        )
        self.assertIn("cargo test --workspace --test '*'", broad)
        nightly = WORKFLOW.read_text()
        sweep = nightly.split("- name: Full workspace sweep including ignored tests", 1)[1]
        sweep = sweep.split("- name: Channel moderation ignored tests with their guarded URL", 1)[0]
        self.assertIn(
            "TWO_TEST_DATABASE_URL: postgres://agent_test:@agent-testdb:5432/two_bot_test_tog10090_nightly",
            sweep,
        )
        self.assertIn("--include-ignored", sweep)

    def test_check_preserves_selector_gating_and_ci_ok_for_ignored_stores(self):
        workflow = CHECK.read_text()
        job = workflow.split("  ignored-db-stores:", 1)[1].split("\n  ignored-db-runtime:", 1)[0]
        self.assertIn("needs: [job-inputs]", job)
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job)
        aggregate = workflow.split("\n  ci-ok:", 1)[1].split("steps:", 1)[0]
        self.assertIn("ignored-db-stores", aggregate)
        # Preserved ignored lib coverage the discord runtime binary does not run.
        self.assertIn(
            "community_store::tests::voice_session_start_end_round_trip_is_idempotent -- --ignored",
            workflow,
        )
        self.assertIn(
            "community_store::tests::message_created_fact_round_trip_is_idempotent -- --ignored",
            workflow,
        )


if __name__ == "__main__":
    unittest.main()

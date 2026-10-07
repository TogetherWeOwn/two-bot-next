from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
CLI_MODULES = {
    "crates/bot/src/backup_cli.rs",
    "crates/bot/src/commands_cli.rs",
    "crates/bot/src/database_roles_cli.rs",
    "crates/bot/src/moderation_cli.rs",
    "crates/bot/src/preflight.rs",
    "crates/core/examples/metrics_rss.rs",
    "crates/core/examples/evidence_reconcile.rs",
    "crates/cutover/src/bin/legacy_copy.rs",
    "crates/cutover/src/bin/legacy_verify.rs",
    "crates/cutover/src/bin/backfill.rs",
    "crates/cutover/src/bin/backfill_messages.rs",
    "crates/cutover/src/bin/capture.rs",
    "crates/cutover/src/bin/dedupe_events.rs",
    "crates/cutover/src/bin/levels_import_mee6.rs",
    "crates/cutover/src/bin/levels_import_rewards_probe.rs",
    "crates/cutover/src/bin/levels_role_rewards.rs",
    "crates/bot/src/erasure_cli.rs",
    "crates/bot/src/member_cli.rs",
    "crates/bot/src/restore_drill.rs",
    "crates/discord/examples/pipeline_bench.rs",
    "crates/cutover/src/bin/audit_switch.rs",
    "crates/cutover/src/bin/gate_stuck_report.rs",
    "crates/cutover/src/bin/gateway_force_identify.rs",
    "crates/cutover/src/bin/ghost_cleanup.rs",
    "crates/cutover/src/bin/preconditions.rs",
    "crates/cutover/src/bin/raid_list.rs",
    "crates/cutover/src/bin/raid_remove.rs",
    "crates/cutover/src/bin/reengagement_list.rs",
    "crates/cutover/src/bin/report.rs",
    "crates/cutover/src/bin/rollback_delta.rs",
    "crates/cutover/src/bin/staging_migrate.rs",
}


class ConsoleLintTests(unittest.TestCase):
    def test_all_packages_inherit_console_lints(self):
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
        clippy = manifest["workspace"]["lints"]["clippy"]
        self.assertEqual(clippy["print_stdout"], "deny")
        self.assertEqual(clippy["dbg_macro"], "deny")
        for path in [ROOT / "Cargo.toml", *(ROOT / "crates").glob("*/Cargo.toml")]:
            with self.subTest(path=path):
                package = tomllib.loads(path.read_text())
                self.assertEqual(package["lints"], {"workspace": True})

    def test_preflight_stdout_is_limited_to_report_and_help(self):
        source = (ROOT / "crates/bot/src/preflight.rs").read_text()
        self.assertNotIn("#![allow(clippy::print_stdout)]", source)
        self.assertEqual(source.count("#[allow(clippy::print_stdout)]"), 2)
        self.assertIn("#[allow(clippy::print_stdout)]\n    fn render(", source)
        self.assertIn("#[allow(clippy::print_stdout)] // CLI help only; preserve exact output bytes.\nfn print_usage()", source)

    def test_stdout_suppressions_are_cli_only_and_dbg_is_not_suppressed(self):
        # Clippy provides the behavioral enforcement. This regression fixture
        # also prevents widening the narrowly documented CLI exceptions.
        for path in (ROOT / "crates").glob("*/**/*.rs"):
            with self.subTest(path=path):
                source = path.read_text()
                attributes = re.findall(r"#!?\[\s*(?:allow|expect)\s*\((.*?)\)\s*\]", source, re.DOTALL)
                for attribute in attributes:
                    self.assertNotIn("clippy::dbg_macro", attribute)
                    if "clippy::print_stdout" in attribute:
                        self.assertIn(path.relative_to(ROOT).as_posix(), CLI_MODULES)


if __name__ == "__main__":
    unittest.main()

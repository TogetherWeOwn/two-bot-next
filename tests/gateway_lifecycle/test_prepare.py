"""Isolated lifecycle-generator regressions; never write into the real crate."""

import importlib.util
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def load_prepare():
    spec = importlib.util.spec_from_file_location("gateway_prepare", HERE / "prepare.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PrepareCleanupTests(unittest.TestCase):
    def setUp(self):
        self.sandbox = tempfile.TemporaryDirectory()
        self.addCleanup(self.sandbox.cleanup)
        self.root = Path(self.sandbox.name)
        self.module = load_prepare()
        self.module.ROOT = self.root
        self.module.TARGET = self.root / "crates/bot/tests/gateway_lifecycle_generated.rs"
        self.module.TARGET.parent.mkdir(parents=True)
        for relative in (
            "crates/bot/src/voice_rooms/game_activity.rs",
            "crates/bot/src/gateway.rs",
            "crates/bot/src/onboarding.rs",
            "crates/bot/src/main.rs",
            "crates/bot/src/server.rs",
            "crates/bot/src/shutdown.rs",
            "crates/bot/src/dispatch.rs",
            "crates/bot/src/eventsub_reconcile.rs",
        ):
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text((ROOT / relative).read_text())

    def test_formatter_failure_removes_owned_target_and_next_prepare_succeeds(self):
        # Break caught: rustfmt raises after the generator writes its target,
        # leaving a stale integration test that blocks the next invocation.
        original = subprocess.CalledProcessError(2, ["rustfmt"])
        with patch.object(self.module.subprocess, "run", side_effect=original):
            with self.assertRaises(subprocess.CalledProcessError) as raised:
                self.module.prepare()
        self.assertIs(raised.exception, original)
        self.assertFalse(self.module.TARGET.exists(), "formatter failure left a generated target")

        with patch.object(self.module.subprocess, "run") as formatter:
            self.module.prepare()
        self.assertTrue(self.module.TARGET.is_file())
        self.assertNotIn("// SCHEDULING_SOURCE", self.module.TARGET.read_text())
        self.assertEqual(formatter.call_count, 1)

    def test_preexisting_target_is_refused_and_never_deleted(self):
        sentinel = "owned by a different invocation\n"
        self.module.TARGET.write_text(sentinel)
        original_inode = self.module.TARGET.stat().st_ino
        with patch.object(self.module.subprocess, "run") as formatter:
            with self.assertRaises(FileExistsError):
                self.module.prepare()
        self.assertEqual(self.module.TARGET.read_text(), sentinel)
        self.assertEqual(self.module.TARGET.stat().st_ino, original_inode)
        formatter.assert_not_called()

    def test_preexisting_target_survives_main_prepare_and_run(self):
        for mode in ("--prepare", "--run"):
            with self.subTest(mode=mode):
                sentinel = "another invocation owns this target\n"
                self.module.TARGET.write_text(sentinel)
                with patch.object(self.module.sys, "argv", ["prepare.py", mode]):
                    with self.assertRaises(FileExistsError):
                        self.module.main()
                self.assertEqual(self.module.TARGET.read_text(), sentinel)

    def test_cleanup_does_not_mask_the_formatter_exception(self):
        original = subprocess.CalledProcessError(2, ["rustfmt"])
        with patch.object(self.module.subprocess, "run", side_effect=original):
            with patch.object(Path, "unlink", side_effect=OSError("injected cleanup failure")):
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    self.module.prepare()
        self.assertIs(raised.exception, original)

    def test_generated_adapter_preserves_all_ten_original_cases(self):
        # This checks the generator's output/collection artifact, not whether
        # production behavior is correct (the Rust cases own those oracles).
        with patch.object(self.module.subprocess, "run"):
            self.module.prepare()
        names = set(re.findall(
            r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*(?:async\s+)?fn\s+(\w+)",
            self.module.TARGET.read_text(),
        ))
        original = {
            "shutdown_persists_one_partial_interval_and_exits",
            "prelatched_shutdown_flushes_without_waiting_for_another_signal",
            "periodic_then_shutdown_counts_each_interval_once",
            "shutdown_during_persist_waits_for_it_without_double_counting",
            "control_hourly_due_check_preserves_cadence_and_drains_once",
            "burst_is_bounded_and_keeps_exactly_the_newest_suffix",
            "oldest_eviction_releases_the_original_ack_ticket",
            "evicted_claim_uses_durable_recovery_without_replaying_credentials",
            "control_retained_claim_preserves_interaction_ticket_and_generation",
            "control_missing_claim_recovers_durable_member_work",
        }
        self.assertTrue(original <= names, sorted(original - names))

    def test_generated_adapter_collects_failure_and_live_composition_cases(self):
        with patch.object(self.module.subprocess, "run"):
            self.module.prepare()
        names = set(re.findall(
            r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*(?:async\s+)?fn\s+(\w+)",
            self.module.TARGET.read_text(),
        ))
        required = {
            "shutdown_retries_each_failure_before_drain",
            "shutdown_retries_requeued_upsert_and_commit_once",
            "exhausted_shutdown_budget_cannot_report_clean_completion",
            "control_gate_failure_keeps_buffers_until_safe_retry",
            "control_retry_rechecks_opt_out_and_independent_voice_gate",
            "control_disabled_guild_discards_buffers_and_carries",
            "control_hourly_failed_write_retries_without_double_counting",
            "service_retains_live_flush_handle_until_trailing_persist_releases",
            "accepted_producer_commit_orders_before_trailing_flush",
            "actual_flush_uses_only_the_service_deadline_remaining_after_producer_drain",
        }
        self.assertTrue(required <= names, sorted(required - names))
        self.assertEqual(len(names), 20, "ten original plus ten added cases must be emitted")

    def test_final_flush_mutant_changes_only_generated_source(self):
        source = self.root / "crates/bot/src/voice_rooms/game_activity.rs"
        original = source.read_bytes()
        with patch.object(self.module.subprocess, "run"):
            self.module.prepare()
        normal = self.module.TARGET.read_text()
        self.module.TARGET.unlink()
        with patch.object(self.module.subprocess, "run"):
            self.module.prepare("drop-final-flush")
        mutated = self.module.TARGET.read_text()
        self.assertNotEqual(normal, mutated)
        self.assertEqual(source.read_bytes(), original)
        with self.assertRaises(ValueError):
            self.module.without_final_flush("no final flush call exists")

    def test_retention_mutation_is_rendered_not_rejected_or_frozen(self):
        main = self.root / "crates/bot/src/main.rs"
        source = main.read_text()
        self.assertEqual(source.count("flush_task.into_iter().collect()"), 1)
        main.write_text(source.replace("flush_task.into_iter().collect()", "Vec::new()", 1))
        with patch.object(self.module.subprocess, "run"):
            self.module.prepare()
        generated = self.module.TARGET.read_text()
        self.assertIn("async fn service_retains_live_flush_handle_until_trailing_persist_releases", generated)
        self.assertNotIn("flush_task.into_iter().collect()", generated)
        self.assertIn("supervise_gateway_bounded", generated)

    def test_prepare_run_failure_cleans_target(self):
        # Break caught: the Cargo subprocess raises, rather than returning a
        # failing status; the --run cleanup must still own its target.
        original = OSError("injected process launch failure")

        def run(command, **kwargs):
            if command[0] == "rustfmt":
                return subprocess.CompletedProcess(command, 0)
            raise original

        with patch.object(self.module.subprocess, "run", side_effect=run):
            with patch.object(self.module.sys, "argv", ["prepare.py", "--run"]):
                with self.assertRaises(OSError) as raised:
                    self.module.main()
        self.assertIs(raised.exception, original)
        self.assertFalse(self.module.TARGET.exists())


if __name__ == "__main__":
    unittest.main()

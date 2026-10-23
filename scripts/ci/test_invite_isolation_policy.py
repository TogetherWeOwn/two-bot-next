"""Mutation controls for the invite fixture's serial-only workflow contract."""

from copy import deepcopy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import test_workflows as policy


class InviteIsolationPolicyTests(unittest.TestCase):
    def setUp(self):
        self.case = policy.WorkflowTests()
        self.case.setUp()

    def assert_isolation_rejected(self):
        # Exercise the actual workflow-policy checks, not another copy of them.
        names = [
            name for name in unittest.defaultTestLoader.getTestCaseNames(policy.WorkflowTests)
            if "invite" in name or name == "test_patch_coverage_runs_with_db_and_includes_ignored_tests"
        ]
        for name in names:
            case = policy.WorkflowTests(name)
            case.workflows = deepcopy(self.case.workflows)
            try:
                getattr(case, name)()
            except AssertionError:
                return
        self.fail("workflow policy accepted an unsafe invite fixture schedule")

    def test_unmodified_workflow_passes_invite_policy(self):
        self.case.test_invite_saturation_isolation_in_both_lanes()

    def test_discovery_refuses_unmapped_tokio_attributes(self):
        for declaration in (
            "#[tokio::test]\npub async fn unknown() {}\n",
            "#[tokio::test]\n#[cfg_attr(\n    feature = \"db\", allow(dead_code)\n)]\nasync fn unknown() {}\n",
            "#[tokio::test]\n/* outer /* nested */ comment */\nasync fn unknown() {}\n",
            "#[tokio::test]",
        ):
            with self.subTest(declaration=declaration):
                with self.assertRaises(ValueError):
                    policy.tokio_test_names("#[tokio::test]\nasync fn ordinary() {}\n" + declaration)

    def test_signal_lane_rejects_parallel_invite_replay(self):
        steps = self.case.workflows["check.yml"]["jobs"]["rust-tests"]["steps"]
        replay = next(step for step in steps if "--test invite_uses_saturation_acceptance" in step.get("run", ""))
        replay["run"] = replay["run"].replace(" --test-threads=1", "")
        self.assert_isolation_rejected()

    def test_signal_lane_rejects_a_narrowed_replay(self):
        steps = self.case.workflows["check.yml"]["jobs"]["rust-tests"]["steps"]
        replay = next(step for step in steps if "--test invite_uses_saturation_acceptance" in step.get("run", ""))
        replay["run"] = replay["run"].replace(" --locked -- ", " --locked normal_uses_round_trip_byte_identical -- ")
        self.assert_isolation_rejected()

    def test_signal_lane_rejects_missing_fixture_skip(self):
        steps = self.case.workflows["check.yml"]["jobs"]["rust-tests"]["steps"]
        sweep = next(step for step in steps if "--workspace --test '*'" in step.get("run", ""))
        sweep["run"] = sweep["run"].replace("--skip uses_u64_max_saturates_at_i32_max", "")
        self.assert_isolation_rejected()

    def test_new_fixture_cannot_hide_behind_attributes_or_trivia(self):
        source_path = policy.ROOT / "crates/store/tests/invite_uses_saturation_acceptance.rs"
        original = source_path.read_text(encoding="utf-8")
        variants = (
            "",
            "#[allow(dead_code)]\n",
            "// explanatory comment\n",
            "/// fixture documentation\n",
            "/* block comment */\n",
            "#[allow(dead_code)]\n// comment\n/* block */\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / source_path.relative_to(policy.ROOT)
            source.parent.mkdir(parents=True)
            for trivia in variants:
                with self.subTest(trivia=trivia):
                    source.write_text(original + "\n#[tokio::test]\n" + trivia + "async fn unskipped_future_fixture() {}\n", encoding="utf-8")
                    real_read = Path.read_text

                    def read_text(path, *args, **kwargs):
                        return real_read(source if path == source_path else path, *args, **kwargs)

                    with patch.object(Path, "read_text", read_text):
                        with self.assertRaises(AssertionError):
                            self.case.test_patch_coverage_runs_with_db_and_includes_ignored_tests()


if __name__ == "__main__":
    unittest.main()

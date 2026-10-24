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

    def policy_result(self, name):
        # TestCase.run records failures inside subTest; calling the method
        # directly can miss them when an outer runner already owns the result.
        case = policy.WorkflowTests(name)
        case.workflows = deepcopy(self.case.workflows)
        result = unittest.TestResult()
        with patch.object(case, "setUp"):
            case.run(result)
        self.assertEqual(result.testsRun, 1)
        self.assertEqual(result.errors, [], result.errors)
        return result

    def assert_isolation_rejected(
        self,
        name="test_invite_saturation_isolation_in_both_lanes",
        jobs=("rust-tests",),
    ):
        result = self.policy_result(name)
        self.assertEqual(len(result.failures), len(jobs), result.failures)
        for job in jobs:
            self.assertTrue(
                any(f"job='{job}'" in case.id() for case, _ in result.failures),
                f"{name} accepted an unsafe schedule in {job}",
            )
        return result

    def test_unmodified_workflow_passes_invite_policy(self):
        for name in (
            "test_invite_saturation_isolation_in_both_lanes",
            "test_patch_coverage_runs_with_db_and_includes_ignored_tests",
        ):
            with self.subTest(policy=name):
                result = self.policy_result(name)
                self.assertTrue(result.wasSuccessful(), result.failures)

    def test_discovery_refuses_unmapped_tokio_attributes(self):
        for declaration in (
            "#[tokio::test]\npub async fn unknown() {}\n",
            "#[tokio::test]\n#[cfg_attr(\n    feature = \"db\", allow(dead_code)\n)]\nasync fn unknown() {}\n",
            "#[tokio::test]\n/* outer /* nested */ comment */\nasync fn unknown() {}\n",
            "#[tokio::test]",
        ):
            for attribute in ("tokio::test", "::tokio::test"):
                with self.subTest(declaration=declaration, attribute=attribute):
                    with self.assertRaises(ValueError):
                        policy.tokio_test_names(
                            "#[tokio::test]\nasync fn ordinary() {}\n"
                            + declaration.replace("tokio::test", attribute),
                        )

    def test_discovery_refuses_unconsumed_tokio_test_tokens(self):
        for source in (
            "use tokio::test as renamed_test;\n#[renamed_test]\nasync fn unknown() {}\n",
            "use tokio::test;\n#[test]\nasync fn unknown() {}\n",
            "#[cfg_attr(all(), tokio::test)]\nasync fn unknown() {}\n",
            "macro_rules! gen {\n"
            "    () => { #[tokio::test] async fn unknown() {} };\n"
            "}\n",
            "#[tokio::test]\nasync fn ordinary() {}\n// historical note: tokio::test\n",
        ):
            with self.subTest(source=source):
                with self.assertRaises(ValueError):
                    policy.tokio_test_names(source)

    def assert_macro_source_rejected(self, declaration):
        source_path = policy.ROOT / "crates/store/tests/invite_uses_saturation_acceptance.rs"
        original = source_path.read_text(encoding="utf-8")
        real_read = Path.read_text

        def read_text(path, *args, **kwargs):
            return original + "\n" + declaration if path == source_path else real_read(path, *args, **kwargs)

        with patch.object(Path, "read_text", read_text):
            for name in (
                "test_invite_saturation_isolation_in_both_lanes",
                "test_patch_coverage_runs_with_db_and_includes_ignored_tests",
            ):
                with self.subTest(policy=name, declaration=declaration):
                    case = policy.WorkflowTests(name)
                    case.workflows = deepcopy(self.case.workflows)
                    result = unittest.TestResult()
                    with patch.object(case, "setUp"):
                        case.run(result)
                    self.assertEqual(result.testsRun, 1)
                    self.assertEqual(result.failures, [], result.failures)
                    self.assertEqual(len(result.errors), 1, result.errors)
                    self.assertIn("unsupported macro source", result.errors[0][1])

    def test_macro_substituted_attribute_fails_closed_in_both_policies(self):
        self.assert_macro_source_rejected(
            "macro_rules! future_fixture {\n"
            "    ($runtime:ident) => {\n"
            "        #[$runtime::test]\n"
            "        async fn unskipped_future_fixture() {}\n"
            "    };\n"
            "}\n"
            "future_fixture!(tokio);\n",
        )

    def test_macro_substituted_import_keyword_fails_closed_in_both_policies(self):
        self.assert_macro_source_rejected(
            "macro_rules! import_runtime {\n"
            "    ($keyword:tt) => { $keyword tokio as rt; };\n"
            "}\n"
            "import_runtime!(use);\n"
            "#[rt::test]\n"
            "async fn unskipped_future_fixture() {}\n",
        )

    def test_local_macro_definitions_fail_closed_without_literal_tokio(self):
        # No invocation is needed: reject the unsupported source boundary,
        # rather than guessing what a definition will eventually transcribe.
        for declaration in (
            "macro_rules! unused { ($r:ident) => { #[$r::test] async fn hidden() {} }; }",
            "macro_rules /* outer /* nested */ trivia */ ! unused { ($t:tt) => { $t }; }",
            "macro_rules\n// trivia\n! unused { ($t:tt) => { $t }; }",
            "macro_rules! r#unused { ($($t:tt)*) => { $($t)* }; }",
            "r#macro_rules! unused { ($t:tt) => { $t }; }",
            "fn helper() { macro_rules! nested { ($t:tt) => { $t }; } }",
            "macro_rules! outer { () => { macro_rules! inner { ($t:tt) => { $t }; } }; }",
            "macro unused($t:tt) { $t }",
        ):
            self.assert_macro_source_rejected(declaration)

    def test_opaque_macro_sources_fail_closed_in_both_policies(self):
        for declaration in (
            'include!("future_fixtures.rs");',
            'include /* trivia */ ! ("future_fixtures.rs");',
            'include! /* outer /* nested */ trivia */ ("future_fixtures.rs");',
            'include! // trivia\n("future_fixtures.rs");',
            '::std::include!("future_fixtures.rs");',
            'r#include!("future_fixtures.rs");',
            "future_fixture!(tokio);",
            "fixture_generators::future_fixture!{tokio}",
            "future_fixture/**/![tokio];",
            "future_fixture! /* trivia */ {tokio}",
            "fixture_generators::format!{tokio}",
            "fixture_generators:: /* trivia */ format!{tokio}",
            "fixture_generators:: // trivia\nformat!{tokio}",
            "fixture_generators:: // trivia ending in a separator;\n\nformat!{tokio}",
            "r#format!(tokio);",
            "x́format!(tokio);",
            "#[macro_use] extern crate fixture_generators;",
            "use fixture_generators::future_fixture as format; format!(tokio);",
            "use fixture_generators::*; format!(tokio);",
        ):
            self.assert_macro_source_rejected(declaration)

    def test_standard_expression_macros_and_doc_comments_remain_supported(self):
        self.assertEqual(
            policy.tokio_test_names(
                "//! Fixture documentation\n//!\n//! More documentation\n"
                "#[tokio::test]\nasync fn ordinary() {\n"
                "    assert!(true); assert_eq!(1, 1); assert_ne!(1, 2);\n"
                '    eprintln!("message"); let _ = format!("message");\n'
                "    let _ = vec![1, 2];\n}\n",
            ),
            ["ordinary"],
        )

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
        attributes = (
            "#[tokio::test]",
            '#[::tokio::test(flavor = "multi_thread", worker_threads = 2)]',
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / source_path.relative_to(policy.ROOT)
            source.parent.mkdir(parents=True)
            for attribute in attributes:
                for trivia in variants:
                    with self.subTest(attribute=attribute, trivia=trivia):
                        source.write_text(
                            original + "\n" + attribute + "\n" + trivia
                            + "async fn unskipped_future_fixture() {}\n",
                            encoding="utf-8",
                        )
                        real_read = Path.read_text

                        def read_text(path, *args, **kwargs):
                            return real_read(source if path == source_path else path, *args, **kwargs)

                        with patch.object(Path, "read_text", read_text):
                            with self.subTest(policy="both lanes"):
                                result = self.assert_isolation_rejected(
                                    jobs=("rust-tests", "patch-coverage"),
                                )
                                for case, _ in result.failures:
                                    self.assertIn("fixture='unskipped_future_fixture'", case.id())
                            with self.subTest(policy="patch coverage"):
                                result = self.policy_result(
                                    "test_patch_coverage_runs_with_db_and_includes_ignored_tests",
                                )
                                self.assertEqual(len(result.failures), 1, result.failures)
                                self.assertIn(
                                    "saturation_skip='unskipped_future_fixture'",
                                    result.failures[0][0].id(),
                                )


    def test_sibling_tokio_spellings_fail_closed_in_both_lanes(self):
        source_path = policy.ROOT / "crates/store/tests/invite_uses_saturation_acceptance.rs"
        original = source_path.read_text(encoding="utf-8")
        declarations = (
            "use tokio::test as renamed_test;\n#[renamed_test]\nasync fn unskipped_future_fixture() {}\n",
            "use tokio::test;\n#[test]\nasync fn unskipped_future_fixture() {}\n",
            "#[cfg_attr(all(), tokio::test)]\nasync fn unskipped_future_fixture() {}\n",
            "macro_rules! gen_future_fixture {\n"
            "    () => { #[tokio::test] async fn unskipped_future_fixture() {} };\n"
            "}\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / source_path.relative_to(policy.ROOT)
            source.parent.mkdir(parents=True)
            for declaration in declarations:
                with self.subTest(declaration=declaration):
                    source.write_text(original + "\n" + declaration, encoding="utf-8")
                    real_read = Path.read_text

                    def read_text(path, *args, **kwargs):
                        return real_read(source if path == source_path else path, *args, **kwargs)

                    with patch.object(Path, "read_text", read_text):
                        for name in (
                            "test_invite_saturation_isolation_in_both_lanes",
                            "test_patch_coverage_runs_with_db_and_includes_ignored_tests",
                        ):
                            with self.subTest(policy=name):
                                case = policy.WorkflowTests(name)
                                case.workflows = deepcopy(self.case.workflows)
                                result = unittest.TestResult()
                                with patch.object(case, "setUp"):
                                    case.run(result)
                                self.assertEqual(result.testsRun, 1)
                                self.assertEqual(result.failures, [], result.failures)
                                self.assertEqual(len(result.errors), 1, result.errors)
                                self.assertIn("tokio::test tokens", result.errors[0][1])


    def test_crate_aliases_and_grouped_imports_fail_closed_in_both_policies(self):
        source_path = policy.ROOT / "crates/store/tests/invite_uses_saturation_acceptance.rs"
        original = source_path.read_text(encoding="utf-8")
        imports = (
            ("use tokio as rt;", "rt::test"),
            ("extern crate tokio as rt;", "rt::test"),
            ("use tokio::{self, test as fixture};", "fixture"),
            ("pub(crate) use ::tokio as rt;", "rt::test"),
            ("pub use {tokio::{self, test as fixture}};", "fixture"),
            ("use tokio::{self as rt};", "rt::test"),
            ("use tokio::*;", "test"),
            ("use /* outer /* ; */ nested */ tokio as rt;", "rt::test"),
            ("extern /* ; */ crate tokio as rt;", "rt::test"),
            ("use tokio /* ; */ :: {self, test as fixture};", "fixture"),
            ("pub use {std::sync::Arc, tokio::{self, test as fixture}};", "fixture"),
        )
        real_read = Path.read_text
        for declaration, attribute in imports:
            source = (
                original + "\n" + declaration + "\n#[" + attribute + "]\n"
                + "async fn unskipped_future_fixture() {}\n"
            )

            def read_text(path, *args, **kwargs):
                return source if path == source_path else real_read(path, *args, **kwargs)

            with patch.object(Path, "read_text", read_text):
                for name in (
                    "test_invite_saturation_isolation_in_both_lanes",
                    "test_patch_coverage_runs_with_db_and_includes_ignored_tests",
                ):
                    with self.subTest(declaration=declaration, policy=name):
                        case = policy.WorkflowTests(name)
                        case.workflows = deepcopy(self.case.workflows)
                        result = unittest.TestResult()
                        with patch.object(case, "setUp"):
                            case.run(result)
                        self.assertEqual(result.testsRun, 1)
                        self.assertEqual(result.failures, [], result.failures)
                        self.assertEqual(len(result.errors), 1, result.errors)
                        self.assertIn("unsupported Tokio import", result.errors[0][1])


if __name__ == "__main__":
    unittest.main()

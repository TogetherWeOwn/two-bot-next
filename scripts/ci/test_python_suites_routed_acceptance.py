"""Acceptance tests: only executable run-step commands route a Python suite.

A suite mentioned only in workflow/step names, env values, comments or
echo/printf text must be reported as unrouted by ``unrouted_suites`` in
``test_python_suites_routed``. Recognized executable forms (direct
``python3 scripts/<suite>`` commands, parent-relative commands and
``unittest discover`` for the suite directory) must keep routing.

These tests intentionally exercise only the stable ``unrouted_suites(root)``
entry point so the guard's internals can be refactored without breaking
this acceptance contract.
"""

from pathlib import Path
import tempfile
import unittest

import test_python_suites_routed as routed

SUITE = "test_fixture_suite.py"
SUITE_PATH = f"scripts/{SUITE}"


def make_fixture(root, workflow_text, suite_name=SUITE):
    """Create a minimal repo tree: one suite file and one workflow."""
    (root / "scripts").mkdir(parents=True)
    (root / "scripts" / suite_name).write_text("")
    workflows = root / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "check.yml").write_text(workflow_text)
    return root


def workflow_with_step(step_lines):
    body = "\n".join(f"        {line}" for line in step_lines)
    return (
        "name: check\n"
        "on: [push]\n"
        "jobs:\n"
        "  check:\n"
        "    runs-on: ubuntu-latest\n"
        "    steps:\n"
        f"{body}\n"
    )


class ExecutableRouteAcceptanceTests(unittest.TestCase):
    def assert_unrouted(self, workflow_text, suite_name=SUITE):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = make_fixture(Path(tmp), workflow_text, suite_name)
            self.assertEqual(routed.unrouted_suites(fixture),
                             [f"scripts/{suite_name}"])

    def assert_routed(self, workflow_text, suite_name=SUITE):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = make_fixture(Path(tmp), workflow_text, suite_name)
            self.assertEqual(routed.unrouted_suites(fixture), [])

    # -- Mentions that must NOT route -------------------------------------

    def test_workflow_name_mention_does_not_route(self):
        self.assert_unrouted(f"name: python3 {SUITE_PATH}\n")

    def test_workflow_name_discover_mention_does_not_route(self):
        self.assert_unrouted(
            "name: python3 -m unittest discover -s scripts "
            f"-p '{SUITE}'\n")

    def test_step_name_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            f"- name: run python3 {SUITE_PATH}",
            "  run: true",
        ]))

    def test_env_value_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            "- env:",
            f"    SUITE_CMD: python3 {SUITE_PATH}",
            "  run: true",
        ]))

    def test_echo_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            f"- run: echo python3 {SUITE_PATH}",
        ]))

    def test_echo_quoted_discover_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            '- run: echo "python3 -m unittest discover -s scripts '
            f"-p '{SUITE}'\"",
        ]))

    def test_printf_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            f"- run: printf '%s\\n' \"python3 {SUITE_PATH}\"",
        ]))

    def test_echo_segment_before_other_command_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            f"- run: echo python3 {SUITE_PATH} && true",
        ]))

    def test_inline_comment_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            f"- run: true # python3 {SUITE_PATH}",
        ]))

    def test_inline_comment_discover_mention_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            "- run: true # python3 -m unittest discover -s scripts "
            f"-p '{SUITE}'",
        ]))

    def test_block_scalar_comment_line_does_not_route(self):
        self.assert_unrouted(workflow_with_step([
            "- run: |",
            f"    # python3 {SUITE_PATH}",
            "    true",
        ]))

    def test_removing_sole_real_command_unroutes_despite_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = make_fixture(Path(tmp), workflow_with_step([
                f"- name: python3 {SUITE_PATH}",
                f"  run: python3 {SUITE_PATH}",
            ]))
            self.assertEqual(routed.unrouted_suites(fixture), [])
            workflow = fixture / ".github/workflows/check.yml"
            workflow.write_text(workflow.read_text().replace(
                f"run: python3 {SUITE_PATH}", "run: true"))
            self.assertEqual(routed.unrouted_suites(fixture), [SUITE_PATH])

    # -- Executable forms that must keep routing --------------------------

    def test_direct_run_command_routes(self):
        self.assert_routed(workflow_with_step([
            f"- run: python3 {SUITE_PATH}",
        ]))

    def test_direct_command_in_block_scalar_routes(self):
        self.assert_routed(workflow_with_step([
            "- run: |",
            f"    python3 {SUITE_PATH}",
        ]))

    def test_parent_relative_direct_command_routes(self):
        self.assert_routed(workflow_with_step([
            f"- run: python3 ../{SUITE_PATH}",
        ]))

    def test_env_assignment_prefix_direct_command_routes(self):
        self.assert_routed(workflow_with_step([
            f"- run: PYTHONDONTWRITEBYTECODE=1 python3 {SUITE_PATH}",
        ]))

    def test_command_after_echo_segment_routes(self):
        self.assert_routed(workflow_with_step([
            f"- run: echo starting && python3 {SUITE_PATH}",
        ]))

    def test_hyphenated_suite_direct_command_routes(self):
        self.assert_routed(
            workflow_with_step([
                "- run: python3 scripts/test-fixture-suite.py",
            ]),
            suite_name="test-fixture-suite.py")

    def test_discover_command_routes(self):
        self.assert_routed(workflow_with_step([
            "- run: python3 -m unittest discover -s scripts "
            f"-p '{SUITE}' -v",
        ]))

    def test_discover_wildcard_in_block_scalar_routes(self):
        self.assert_routed(workflow_with_step([
            "- run: |",
            "    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover "
            "-s scripts -p 'test_fixture_*.py' -v",
        ]))

    def test_discover_parent_relative_directory_routes(self):
        self.assert_routed(workflow_with_step([
            "- run: python3 -m unittest discover -s ../scripts "
            f"-p '{SUITE}'",
        ]))

    def test_real_repository_suites_stay_routed(self):
        self.assertEqual(routed.unrouted_suites(routed.ROOT), [])


if __name__ == "__main__":
    unittest.main()

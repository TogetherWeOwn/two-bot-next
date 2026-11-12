"""Static role-instruction regressions, not runtime qualification or enforcement."""

from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


def load_role(name):
    text = (ROOT / ".claude" / "agents" / f"{name}.md").read_text()
    _, frontmatter, body = text.split("---", 2)
    return yaml.safe_load(frontmatter), body


class AgentRoleContractTests(unittest.TestCase):
    def test_existing_routes_and_tools_are_preserved(self):
        expected = {
            "cross-reviewer": ("gpt-6.1-sol", "Read, Grep, Glob, Bash"),
            "builder": ("claude/claude-sonnet-5-5", "Read, Grep, Glob, Bash, Edit, Write"),
        }
        for name, (model, tools) in expected.items():
            with self.subTest(role=name):
                metadata, _ = load_role(name)
                self.assertEqual(metadata["name"], name)
                self.assertEqual(metadata["model"], model)
                self.assertEqual(metadata["tools"], tools)
                self.assertEqual(set(metadata), {"name", "description", "model", "tools"})

    def test_cross_review_is_independent_concurrent_and_pinned(self):
        metadata, body = load_role("cross-reviewer")
        self.assertIn("alongside the primary assessment", metadata["description"])
        for requirement in (
            "independently and concurrently",
            "same pinned head",
            "base and head commit SHAs",
            "Withhold preliminary conclusions until independent findings are recorded",
            "reconcile before decisions",
            "changed head requires applicable revalidation",
            "concrete failing input or scenario",
            "the evidence establishing it",
            "unverified hypotheses",
            "pre-existing findings",
        ):
            with self.subTest(requirement=requirement):
                self.assertIn(requirement, body)
        self.assertNotIn("after the main-thread review", metadata["description"])

    def test_cross_review_does_not_execute_tests_or_claim_a_sandbox(self):
        _, body = load_role("cross-reviewer")
        self.assertIn("Do not edit files, run tests or builds, install dependencies", body)
        self.assertIn("not a security boundary", body)
        self.assertIn("checks not run; never imply test execution", body)
        self.assertNotIn("running existing tests", body)

    def test_builder_accepts_substantial_scoped_work_without_new_specialism(self):
        metadata, body = load_role("builder")
        self.assertIn("substantial implementation", metadata["description"])
        for requirement in (
            "within your verified qualifications and authorization",
            "Size alone does not require work to stay on the parent",
            "keep small cohesive work together",
            "metadata alone does not prove qualification",
            "Architecture, debugging, primary code review and specialist decisions",
            "No design work or Security or Release Manager authority is delegated",
            "caller remains the accountable integrator",
            "file/output ownership and integration boundaries before parallel writes",
            "verified native task-owned isolated workspaces",
            "Source isolation does not isolate shared databases",
            "verified native, provider, tool, host and approved-budget capacity",
            "full suites and required checks on GitHub Actions for the exact head",
            "never run a local full suite or CI mirror",
            "one failing test or crate",
            "Never commit, push, open PRs or change Paperclip issues",
        ):
            with self.subTest(requirement=requirement):
                self.assertIn(requirement, body)

    def test_both_roles_keep_native_gates_and_experimental_no_go(self):
        for name in ("builder", "cross-reviewer"):
            _, body = load_role(name)
            for requirement in (
                "active-participant authority", "stage/participant IDs",
                "executor exclusion", "review rounds", "release gates",
                "never pass an Agent model override", "runtime settings",
                "Experimental review-panel, annotated-diff and hill-climb work remains NO-GO",
            ):
                with self.subTest(role=name, requirement=requirement):
                    self.assertIn(requirement, body)


if __name__ == "__main__":
    unittest.main()

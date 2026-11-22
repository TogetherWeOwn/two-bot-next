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
    def test_claude_code_models_and_tools(self):
        expected = {
            "Explore": ("haiku", "Read, Grep, Glob, Bash"),
            "scout": ("haiku", "Read, Grep, Glob, Bash"),
            "cross-reviewer": ("opus", "Read, Grep, Glob, Bash"),
            "builder": ("sonnet", "Read, Grep, Glob, Bash, Edit, Write"),
        }
        for name, (model, tools) in expected.items():
            with self.subTest(role=name):
                metadata, _ = load_role(name)
                self.assertEqual(metadata["name"], name)
                self.assertEqual(metadata["model"], model)
                self.assertEqual(metadata["tools"], tools)
                self.assertEqual(set(metadata), {"name", "description", "model", "tools"})

    def test_cross_review_is_independent_and_pinned(self):
        metadata, body = load_role("cross-reviewer")
        self.assertIn("authored by a different model family", metadata["description"])
        for requirement in (
            "risk-tier policy in CONTRIBUTING.md",
            "base and published head SHAs",
            "record findings before reading any other review of that head",
            "Decline cross-model review if the executed model is from the author's model family",
            "The caller verifies model identity",
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
            "No design, security approval or production approval authority is delegated",
            "caller remains the accountable integrator",
            "file/output ownership and integration boundaries before parallel writes",
            "verified native task-owned isolated workspaces",
            "Source isolation does not isolate shared databases",
            "verified native, provider, tool, host and approved-budget capacity",
            "full suites and required checks on GitHub Actions for the exact head",
            "never run a local full suite or CI mirror",
            "one failing test or crate",
            "Leave commits, pushes and PR management to the caller",
        ):
            with self.subTest(requirement=requirement):
                self.assertIn(requirement, body)

    def test_both_roles_keep_model_and_release_controls(self):
        for name in ("builder", "cross-reviewer"):
            _, body = load_role(name)
            for requirement in (
                "required checks", "release gates",
                "Never override this Claude Code role's model",
                "runtime settings, tools and permissions",
                "Experimental review-panel, annotated-diff and hill-climb work remains on hold",
            ):
                with self.subTest(role=name, requirement=requirement):
                    self.assertIn(requirement, body)


if __name__ == "__main__":
    unittest.main()

"""Offline declaration checks, not runtime qualification of reviewer isolation."""

from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
ROLES = {"panel-opus": "claude-opus-5-5", "cross-reviewer": "gpt-6.1-sol"}
DENIED = "Agent, Task, Bash, Edit, Write, NotebookEdit"


class PanelRoleTests(unittest.TestCase):
    def test_roles_pin_models_and_effort_without_extra_capabilities(self):
        for name, model in ROLES.items():
            with self.subTest(role=name):
                text = (ROOT / ".claude/agents" / f"{name}.md").read_text()
                opening, header, body = text.split("---", 2)
                self.assertEqual(opening, "")
                metadata = yaml.load(header, Loader=yaml.BaseLoader)
                self.assertTrue(metadata["description"].strip())
                self.assertEqual(metadata, {
                    "name": name,
                    "description": metadata["description"],
                    "model": model,
                    "effort": "xhigh",
                    "tools": "Read, Grep, Glob",
                    "disallowedTools": DENIED,
                })
                self.assertIn("Do not start subagents", body)
                self.assertIn("not run:", body)
                self.assertIn("docs/review-panel-roles.md", body)

    def test_protocol_requires_evidence_and_preserves_qualification_boundary(self):
        text = (ROOT / "docs/review-panel-roles.md").read_text()
        for required in (
            "REVIEWED: <sha>", "SCORE:", "VERDICT:", "ship-with-changes",
            "no score", "not run", "native SHA-pinned", "served model",
            "not a sandbox", "no-delegation", "NOT QUALIFIED",
        ):
            with self.subTest(required=required):
                self.assertIn(required, text)


if __name__ == "__main__":
    unittest.main()

"""Check AGENTS.md reply examples with the pinned org-required workflow.

Supply the public workflow file from TogetherWeOwn/.github at
97fc0e24038021dfaa1fc9da754c6c5d6429130f (the main ruleset pin):
    PR_STANDARDS_WORKFLOW=/path/to/pr-standards-required.yml python3 scripts/test_pr_review_reply_examples.py -v
"""

import hashlib
import json
import os
import re
import subprocess
import sys
import textwrap
import unittest
from pathlib import Path

WORKFLOW_SHA256 = "c7e6135927c54069114e1549541fb0ad116c7181e7961106e33d0db4b3bcb2a2"
GUIDE = Path(__file__).resolve().parents[1] / "AGENTS.md"


class ReviewReplyExamples(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow_path = os.environ.get("PR_STANDARDS_WORKFLOW")
        if not workflow_path:
            raise ValueError("Set PR_STANDARDS_WORKFLOW to the local pinned workflow file")
        data = Path(workflow_path).read_bytes()
        if hashlib.sha256(data).hexdigest() != WORKFLOW_SHA256:
            raise ValueError("PR_STANDARDS_WORKFLOW does not match the required workflow pin")
        workflow = data.decode("utf-8")
        script = re.search(r"<<'PYEOF2'\n(.*?)\n\s*PYEOF2\n", workflow, re.S)
        cls.validator = textwrap.dedent(script.group(1))
        cls.enforce_since = re.search(r'REVIEW_ENFORCE_SINCE: "([^"]+)"', workflow).group(1)

    def examples(self):
        _, heading, rest = GUIDE.read_text(encoding="utf-8").partition("### Review replies\n")
        self.assertTrue(heading, "AGENTS.md needs review reply templates")
        section = rest.split("\n## ", 1)[0]
        examples = re.findall(r"^```markdown\n(.*?)^```$", section, re.M | re.S)
        self.assertEqual(len(examples), 2)
        return examples

    def validate(self, reply):
        thread = [
            {"login": "devin-ai-integration", "body": "Synthetic finding"},
            {"login": "example-author", "body": reply},
        ]
        return subprocess.run(
            [sys.executable, "-I", "-c", self.validator],
            env={
                "EVENT": "pull_request",
                "AUTHOR": "example-author",
                "CREATED_AT": "2026-11-25T00:00:00Z",
                "DRAFT": "false",
                "REPO": "example/repo",
                "HEAD_REPO": "example/repo",
                "REVIEW_ENFORCE_SINCE": self.enforce_since,
                "THREADS_JSON": json.dumps([thread]),
            },
            capture_output=True,
            text=True,
            timeout=10,
        )

    def test_fixed_reply(self):
        fixed, _ = self.examples()
        self.assertTrue(fixed.startswith("Fixed in "))
        result = self.validate(fixed)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("answered in format: 1", result.stdout)

    def test_disagreed_reply(self):
        _, disagreed = self.examples()
        self.assertTrue(disagreed.startswith("Rejected: "))
        result = self.validate(disagreed)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("answered in format: 1", result.stdout)

    def test_attribution_first_is_rejected(self):
        for example in self.examples():
            first_line, attribution, *details = example.strip().split("\n\n")
            with self.subTest(first_line=first_line):
                self.assertTrue(attribution.startswith("> [!NOTE]\n> 🤖 "))
                result = self.validate("\n\n".join([attribution, first_line, *details]))
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("no reply starts with a counted first line", result.stdout)


if __name__ == "__main__":
    unittest.main()

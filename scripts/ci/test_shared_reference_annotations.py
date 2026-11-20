"""Pin the verified version annotations beside shared immutable references."""

from pathlib import Path
import re
import unittest


WORKFLOWS = Path(__file__).resolve().parents[2] / ".github/workflows"


class SharedReferenceAnnotationTests(unittest.TestCase):
    def test_verified_annotations_match_pinned_references(self):
        references = (
            ("daily-digest.yml", "actions/calver", "2ef8ad6447e11d019ac3098bcfd011193b75266a", "v0.10.0", 2),
            ("review-rules.yml", ".github/workflows/review-rules.yml", "2ef8ad6447e11d019ac3098bcfd011193b75266a", "v0.10.0", 1),
            ("test-guard.yml", ".github/workflows/test-guard.yml", "2ef8ad6447e11d019ac3098bcfd011193b75266a", "v0.10.0", 1),
        )
        for filename, path, sha, version, count in references:
            with self.subTest(workflow=filename):
                text = (WORKFLOWS / filename).read_text()
                pattern = rf"^\s*(?:-\s*)?uses:\s*TogetherWeOwn/\.github/{re.escape(path)}@([^\s]+)(?:\s+#\s*(\S+))?\s*$"
                self.assertEqual(re.findall(pattern, text, re.MULTILINE), [(sha, version)] * count)

    def test_size_callers_keep_distinct_immutable_workflows(self):
        text = (WORKFLOWS / "pr-size.yml").read_text()
        calls = re.findall(
            r"^\s*uses:\s*TogetherWeOwn/\.github/\.github/workflows/"
            r"(pr-size(?:-gate|-weekly)?\.yml)@([^\s]+)",
            text,
            re.MULTILINE,
        )
        sha = "ac9ce8d63e0cebd19ede904ac79137190c329964"
        self.assertCountEqual(calls, [
            ("pr-size.yml", sha),
            ("pr-size-gate.yml", sha),
            ("pr-size-weekly.yml", sha),
        ])

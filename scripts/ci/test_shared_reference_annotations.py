"""Pin the verified version annotations beside shared immutable references."""

from pathlib import Path
import re
import unittest


WORKFLOWS = Path(__file__).resolve().parents[2] / ".github/workflows"


class SharedReferenceAnnotationTests(unittest.TestCase):
    def test_verified_annotations_match_pinned_references(self):
        references = (
            ("daily-digest.yml", "actions/calver", "2f0f9667c091e5d877b343d4f722a30b96124561", "v0.3.1", 2),
            ("review-rules.yml", ".github/workflows/review-rules.yml", "63699075b492914c137281acffd4c89e51cc1c4c", "v0.2.0", 1),
            ("test-guard.yml", ".github/workflows/test-guard.yml", "3fffeb9f9465f371763fcc3c789ed4baefe38f95", "v0.0.1", 1),
        )
        for filename, path, sha, version, count in references:
            with self.subTest(workflow=filename):
                text = (WORKFLOWS / filename).read_text()
                pattern = rf"^\s*(?:-\s*)?uses:\s*TogetherWeOwn/\.github/{re.escape(path)}@([^\s]+)(?:\s+#\s*(\S+))?\s*$"
                self.assertEqual(re.findall(pattern, text, re.MULTILINE), [(sha, version)] * count)

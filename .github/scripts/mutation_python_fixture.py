"""Actions-only fixtures for Python mutation scope and unittest verdicts."""

import os
import tempfile
from pathlib import Path

from mutation_python import measure, snapshot, verdict
from mutation_rust_fixture import commit
from mutation_scope import added_lines, git, pr_diff, python_lines

MODULE = "scripts/rule.py"
TESTS = "scripts/test_rule.py"
OLD = "def legacy(n): return n > 99\ndef eligible(n): return n > 3\n"
NEW = OLD.replace("n > 3", "n >= 3")
HEADER = "import unittest\nfrom rule import eligible, legacy\n\nclass RuleTests(unittest.TestCase):\n"


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("mutation fixtures run only in GitHub Actions")
    with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as scratch:
        root = Path(scratch, "repo")
        root.mkdir()
        git(root, "init", "-q")
        (root / "scripts").mkdir()
        module, tests = root / MODULE, root / TESTS
        module.write_text(OLD)
        base = commit(root, "baseline")
        module.write_text(NEW)
        for label, test, status, expected in (
            ("strong", "def test_boundary(self): self.assertTrue(eligible(3))", "killed", 0),
            ("weak", "def test_legacy(self): self.assertTrue(legacy(100))", "survived", 1),
            ("broken", "def test_broken(self): self.assertFalse(eligible(3))", "infrastructure_error", 2),
            ("empty", "pass", "unsupported", 0),
        ):
            tests.write_text(HEADER + "    " + test + "\n")
            head = commit(root, label)
            _, patch = pr_diff(root, base, head)
            changed = python_lines(added_lines(patch))
            assert changed == {MODULE: {2}}, changed
            with tempfile.TemporaryDirectory(dir=scratch) as temporary:
                copy = snapshot(root, Path(temporary))
                results = measure(copy, changed, {MODULE: (TESTS,)})
                assert (copy / MODULE).read_text() == NEW
            assert len(results) == 1 and results[0]["status"] == status, results
            if status in ("killed", "survived"):
                assert results[0]["line"] == 2, results
            assert verdict({"results": results}, "hard") == expected
            assert verdict({"results": results}, "warn") == 0
            print(f"Python fixture {label}: {status}, HARD exit {expected}")


if __name__ == "__main__":
    main()

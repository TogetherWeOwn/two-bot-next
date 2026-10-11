"""Every offline Python suite under scripts/ must be routed to a workflow.

CI names suites one file at a time (`unittest discover -s scripts -p
'test_<name>.py'`); a few use a wildcard pattern. A new `scripts/test_*.py`
that no workflow pattern matches never runs, so a drifted guard stays green.
A suite that a step runs directly by path (`python3 scripts/test-<name>.py`,
`python3 ../scripts/test_<name>.py`) is routed too: every hyphenated
`scripts/test-*.py` file and the worker-lane suites are run that way.
"""

import fnmatch
from pathlib import Path
import re
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
DISCOVER = re.compile(r"unittest\s+discover\b(?P<args>[^\n]*)")
SUITE_DIR = re.compile(r"""(?:^|\s)-s\s+(?P<q>['"]?)(?P<dir>[^\s'"]+)(?P=q)""")
SUITE_PATTERN = re.compile(r"""(?:^|\s)-p\s+(?P<q>['"]?)(?P<pat>[^\s'"]+)(?P=q)""")


def workflow_commands(root):
    """Workflow text with comment-only lines removed, so a mention is not a run."""
    lines = []
    for path in sorted((root / ".github/workflows").glob("*.y*ml")):
        lines += [line for line in path.read_text().splitlines()
                  if not line.lstrip().startswith("#")]
    return "\n".join(lines)


def discover_patterns(commands, suite_dir):
    """Patterns of every `unittest discover` that scans `suite_dir`."""
    found = []
    for match in DISCOVER.finditer(commands):
        args = " " + match["args"]
        directory = SUITE_DIR.search(args)
        pattern = SUITE_PATTERN.search(args)
        if directory and pattern:
            normalized = re.sub(r"^(?:\.\.?/)+", "", directory["dir"]).rstrip("/")
            if normalized == suite_dir:
                found.append(pattern["pat"])
    return found


def unrouted_suites(root):
    commands = workflow_commands(root)
    patterns = discover_patterns(commands, "scripts")
    missing = []
    for path in sorted((root / "scripts").glob("test[_-]*.py")):
        name = path.name
        if any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns):
            continue
        if re.search(r"\bpython3\s+(?:-\S+\s+)*(?:\.\./)?scripts/" + re.escape(name) + r"(?!\w)", commands):
            continue
        missing.append(f"scripts/{name}")
    return missing


class PythonSuiteRoutingTests(unittest.TestCase):
    def test_every_script_suite_is_routed_to_a_workflow(self):
        self.assertEqual(unrouted_suites(ROOT), [],
                         "add a `unittest discover -s scripts -p '<file>'` step to a workflow")

    def test_the_guard_reads_the_real_patterns(self):
        patterns = discover_patterns(workflow_commands(ROOT), "scripts")
        self.assertIn("test_rollback_readiness_probe.py", patterns)
        self.assertIn("test_cutover_freeze_drill.py", patterns)
        self.assertIn("test*pipeline_bench*.py", patterns)
        self.assertEqual(discover_patterns(workflow_commands(ROOT), "scripts/ci"), ["test_*.py"])

    def test_a_fixture_with_an_unrouted_suite_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Path(tmp)
            shutil.copytree(ROOT / ".github/workflows", fixture / ".github/workflows")
            shutil.copytree(ROOT / "scripts", fixture / "scripts",
                            ignore=shutil.ignore_patterns("__pycache__", "ci", "fixtures"))
            self.assertEqual(unrouted_suites(fixture), [])
            (fixture / "scripts/test_never_routed.py").write_text("")
            (fixture / "scripts/test-never-routed.py").write_text("")
            self.assertEqual(unrouted_suites(fixture),
                             ["scripts/test-never-routed.py", "scripts/test_never_routed.py"])
            # A wildcard pattern routes new files; a comment mention does not.
            workflow = fixture / ".github/workflows/check.yml"
            workflow.write_text(workflow.read_text()
                                + "      # unittest discover -s scripts -p 'test_never_*.py'\n")
            self.assertEqual(len(unrouted_suites(fixture)), 2)
            workflow.write_text(workflow.read_text()
                                + "      - run: python3 -m unittest discover -s scripts -p 'test_never_*.py' -v\n")
            self.assertEqual(unrouted_suites(fixture), ["scripts/test-never-routed.py"])
            workflow.write_text(workflow.read_text()
                                + "      - run: python3 scripts/test-never-routed.py\n")
            self.assertEqual(unrouted_suites(fixture), [])

    def test_removing_a_routed_step_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Path(tmp)
            shutil.copytree(ROOT / ".github/workflows", fixture / ".github/workflows")
            shutil.copytree(ROOT / "scripts", fixture / "scripts",
                            ignore=shutil.ignore_patterns("__pycache__", "ci", "fixtures"))
            workflow = fixture / ".github/workflows/check.yml"
            text = workflow.read_text()
            self.assertIn("-p 'test_rollback_readiness_probe.py'", text)
            workflow.write_text(text.replace("-p 'test_rollback_readiness_probe.py'", "-p 'test_other.py'"))
            self.assertEqual(unrouted_suites(fixture), ["scripts/test_rollback_readiness_probe.py"])


if __name__ == "__main__":
    unittest.main()

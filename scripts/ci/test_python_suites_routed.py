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

_RUN_RE = re.compile(r"^(?P<prefix>[ ]*(?:-[ ]+)?)run:[ ]*(?P<value>.*)$")
_BLOCK_SCALAR_RE = re.compile(r"^[|>][+-]?[0-9]*$")

# Bounded run-step extraction, mirroring the check-test-inventory.py
# executable-command precedent: only `run:` values count, comments and
# echo/printf prints are stripped, no shell/YAML interpreter.


def extract_run_blocks(text):
    """Return the shell text of each ``run:`` value, one entry per step."""
    lines = text.splitlines()
    blocks = []
    index = 0
    while index < len(lines):
        match = _RUN_RE.match(lines[index])
        index += 1
        if match is None:
            continue
        value = match.group("value").strip()
        marker = value.split("#", 1)[0].strip() if value else ""
        if marker and _BLOCK_SCALAR_RE.match(marker):
            prefix = match.group("prefix")
            parent_col = len(prefix)
            body = []
            while index < len(lines):
                line = lines[index]
                if not line.strip():
                    body.append("")
                    index += 1
                    continue
                indent = len(line) - len(line.lstrip(" "))
                if indent <= parent_col:
                    break
                body.append(line)
                index += 1
            if marker.startswith(">"):
                folded, pending = [], ""
                for line in body:
                    if line.strip():
                        pending = f"{pending} {line.strip()}".strip()
                    elif pending:
                        folded.append(pending)
                        pending = ""
                if pending:
                    folded.append(pending)
                body = folded
            blocks.append(body)
        elif value:
            blocks.append([match.group("value")])
    return blocks


def _logical_lines(body):
    """Join backslash-continued shell lines into single logical lines."""
    logical = []
    pending = ""
    for line in body:
        stripped = line.rstrip()
        if stripped.endswith("\\") and not stripped.endswith("\\\\"):
            pending += stripped[:-1] + " "
            continue
        logical.append(pending + line)
        pending = ""
    if pending:
        logical.append(pending)
    return logical


def _strip_inline_comment(line):
    in_single = in_double = False
    escaped = False
    for position, char in enumerate(line):
        if escaped:
            escaped = False
            continue
        if char == "\\" and not in_single:
            escaped = True
            continue
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif char == "#" and not in_single and not in_double and (
                position == 0 or line[position - 1] in " \t;|&"):
            return line[:position]
    return line


def _split_segments(line):
    """Split a logical line on ``;``, ``&&``, ``||`` and ``|`` outside quotes."""
    segments = []
    start = 0
    depth = 0
    in_single = in_double = False
    escaped = False
    index = 0
    while index < len(line):
        char = line[index]
        if escaped:
            escaped = False
        elif char == "\\" and not in_single:
            escaped = True
        elif char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif not in_single and not in_double:
            if char == "(":
                depth += 1
            elif char == ")":
                depth = max(0, depth - 1)
            elif depth == 0 and char == ";":
                segments.append(line[start:index])
                start = index + 1
            elif depth == 0 and line.startswith(("&&", "||"), index):
                segments.append(line[start:index])
                start = index + 2
                index += 1
            elif depth == 0 and char == "|":
                segments.append(line[start:index])
                start = index + 1
        index += 1
    segments.append(line[start:])
    return segments


_WRAPPER_WORDS = ("do", "then", "else", "time", "!", "sudo", "command",
                  "builtin", "env")
_TOKEN_RE = re.compile(r"^\s*(\S+)")


def _is_print_segment(segment):
    """True for echo/printf commands, which print a name without running it."""
    rest = segment
    after_wrapper = False
    while True:
        match = _TOKEN_RE.match(rest)
        if match is None:
            return False
        token = match.group(1).lstrip("({!")
        rest = rest[match.end():]
        if token in _WRAPPER_WORDS:
            after_wrapper = token in ("sudo", "env")
            continue
        if re.match(r"[A-Za-z_][\w]*=", token):
            continue
        if after_wrapper and token.startswith("-"):
            continue
        return token.rsplit("/", 1)[-1] in ("echo", "printf")


def executable_block_text(body):
    """Shell text stripped of comments and echo/printf prints."""
    kept = []
    for line in _logical_lines(body):
        line = _strip_inline_comment(line)
        for segment in _split_segments(line):
            if not _is_print_segment(segment):
                kept.append(segment)
    return "\n".join(kept)


def workflow_commands(root):
    """Executable run-step shell text, so a mention is not a run."""
    texts = []
    for path in sorted((root / ".github/workflows").glob("*.y*ml")):
        blocks = extract_run_blocks(path.read_text())
        texts.extend(executable_block_text(body) for body in blocks)
    return "\n".join(texts)


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
            # The probe is dual-routed: a discover step in the check job and a
            # direct `python3 ../scripts/...` step in the worker job. Removing
            # only one routing must not report it as unrouted.
            unrouted_text = (
                text.replace("-p 'test_rollback_readiness_probe.py'", "-p 'test_other.py'")
                .replace("scripts/test_rollback_readiness_probe.py",
                         "scripts/test_other.py")
            )
            workflow.write_text(unrouted_text)
            self.assertEqual(unrouted_suites(fixture), ["scripts/test_rollback_readiness_probe.py"])


if __name__ == "__main__":
    unittest.main()

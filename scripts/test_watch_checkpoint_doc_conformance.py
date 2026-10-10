"""Watch-checkpoint doc invocation conformance: offline, no network or databases.

Pins `docs/cutover-sequence.md` section 5 to the
`scripts/cutover_watch_checkpoint.py` argparse so flag drift fails CI.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOC = ROOT / "docs/cutover-sequence.md"
SCRIPT = ROOT / "scripts/cutover_watch_checkpoint.py"

EXPECTED_FLAGS = ("--checkpoint", "--expected-sha", "--expected-build-id", "--production-url")
EXPECTED_CHECKPOINTS = ("+15m", "+1h", "+6h", "+24h", "+48h")
FLAG_RE = re.compile(r"--[a-z0-9][a-z0-9-]*")
ARG_RE = re.compile(r'add_argument\(\s*"(--[^"]+)"')
FENCE_RE = re.compile(r"```sh(.*?)```", re.DOTALL)


def section5_text(doc_text=None):
    text = doc_text if doc_text is not None else DOC.read_text()
    start = text.index("## 5. Watch handoff")
    try:
        end = text.index("\n## ", start + len("## 5. Watch handoff"))
    except ValueError:
        end = len(text)
    return text[start:end]


def section5_fences(section=None):
    section = section if section is not None else section5_text()
    return FENCE_RE.findall(section)


def doc_invocation_text(section=None):
    fences = section5_fences(section)
    hits = [block for block in fences if "cutover_watch_checkpoint.py" in block]
    assert hits, "section 5 names no cutover_watch_checkpoint.py invocation block"
    return hits[0]


def flags_in_order(text):
    return FLAG_RE.findall(text)


def script_flags(script_text=None):
    text = script_text if script_text is not None else SCRIPT.read_text()
    return tuple(ARG_RE.findall(text))


def check_invocation(doc_flags, code_flags):
    """Fail named when the doc invocation drifts from the script argparse."""
    doc_set, code_set = set(doc_flags), set(code_flags)
    missing = sorted(set(code_set) - set(doc_set))
    extra = sorted(set(doc_set) - set(code_set))
    if missing or extra:
        raise AssertionError(
            "watch-checkpoint doc invocation drifted from script argparse "
            f"(missing from doc: {missing or []}; "
            f"extra in doc: {extra or []}; "
            f"script argparse: {sorted(code_set)})"
        )


class WatchCheckpointDocConformanceTests(unittest.TestCase):
    def test_section5_invocation_flags_match_script_argparse(self):
        invocation = doc_invocation_text()
        doc_flags = flags_in_order(invocation)
        code_flags = script_flags()
        self.assertEqual(tuple(doc_flags), EXPECTED_FLAGS,
                         "doc section 5 invocation must list the four flags in argparse order")
        self.assertEqual(tuple(code_flags), EXPECTED_FLAGS,
                         "script argparse must keep exactly the four documented flags")
        check_invocation(doc_flags, code_flags)

    def test_section5_states_one_checkpoint_per_call(self):
        section = section5_text()
        collapsed = re.sub(r"\s+", " ", section)
        self.assertIn("one checkpoint per call", collapsed)
        invocation = doc_invocation_text(section)
        self.assertEqual(invocation.count("--checkpoint"), 1,
                         "doc invocation records one checkpoint per call")
        source = SCRIPT.read_text()
        self.assertNotIn("nargs", source,
                         "script takes a single --checkpoint per invocation, never nargs")

    def test_section5_checkpoint_labels_match_script(self):
        section = section5_text()
        for label in EXPECTED_CHECKPOINTS:
            self.assertIn(label, section,
                          f"doc section 5 names no {label} checkpoint row")
        import cutover_watch_checkpoint as checkpoint
        self.assertEqual(tuple(checkpoint.CHECKPOINTS), EXPECTED_CHECKPOINTS)
        invocation = doc_invocation_text(section)
        self.assertIn("--checkpoint", invocation)

    def test_section5_read_only_contract_matches_script(self):
        section = section5_text()
        collapsed = re.sub(r"\s+", " ", section)
        self.assertIn("read-only", collapsed)
        self.assertIn("one GET to `/readyz`", collapsed)
        self.assertIn("ROLLBACK decision stays human", section)
        source = SCRIPT.read_text()
        for lib in ("sqlite3", "psycopg", "sqlalchemy", "sqlx"):
            self.assertNotIn(lib, source,
                             f"watch checkpoint must stay read-only: no {lib}")
        self.assertIn("only network call is one GET", source)
        self.assertIn("fetch_one(endpoint)", source)

    def test_deliberate_drift_fixture_fails_named(self):
        code_flags = list(script_flags())
        # Renamed flag: --expected-sha shortened to --sha.
        renamed = ["--checkpoint", "--sha", "--expected-build-id", "--production-url"]
        with self.assertRaisesRegex(AssertionError, r"--expected-sha.*--sha|--sha.*--expected-sha"):
            check_invocation(renamed, code_flags)
        # Dropped flag: --production-url missing.
        dropped = ["--checkpoint", "--expected-sha", "--expected-build-id"]
        with self.assertRaisesRegex(AssertionError, "--production-url"):
            check_invocation(dropped, code_flags)
        # Added flag: --verbose never existed in argparse.
        added = [*EXPECTED_FLAGS, "--verbose"]
        with self.assertRaisesRegex(AssertionError, "--verbose"):
            check_invocation(added, code_flags)


if __name__ == "__main__":
    unittest.main()

"""Watch-checkpoint doc invocation conformance: offline, no network or databases.

Pins the `scripts/cutover_watch_checkpoint.py` invocation blocks in
`docs/cutover-sequence.md` section 5 AND `docs/production-deploy.md`
"48-hour watch log" (the two copies operators read) to the script
argparse, so flag drift in either copy fails CI. Every fence that names
the script is checked, not just the first.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOC = ROOT / "docs/cutover-sequence.md"
PROD_DOC = ROOT / "docs/production-deploy.md"
SCRIPT = ROOT / "scripts/cutover_watch_checkpoint.py"
PINNED_SECTIONS = (
    (DOC, "## 5. Watch handoff"),
    (PROD_DOC, "## 48-hour watch log"),
)

EXPECTED_FLAGS = ("--checkpoint", "--expected-sha", "--expected-build-id", "--production-url")
EXPECTED_CHECKPOINTS = ("+15m", "+1h", "+6h", "+24h", "+48h")
FLAG_RE = re.compile(r"--[a-z0-9][a-z0-9-]*")
ARG_RE = re.compile(r'add_argument\(\s*"(--[^"]+)"')
FENCE_RE = re.compile(r"```sh(.*?)```", re.DOTALL)


def section_text(doc_path, heading, doc_text=None):
    text = doc_text if doc_text is not None else doc_path.read_text()
    start = text.index(heading)
    try:
        end = text.index("\n## ", start + len(heading))
    except ValueError:
        end = len(text)
    return text[start:end]


def section5_text(doc_text=None):
    return section_text(DOC, "## 5. Watch handoff", doc_text)


def invocation_blocks(section):
    """Every fence in a section that names the script (never just the first)."""
    return [block for block in FENCE_RE.findall(section)
            if "cutover_watch_checkpoint.py" in block]


def section5_fences(section=None):
    section = section if section is not None else section5_text()
    return FENCE_RE.findall(section)


def doc_invocation_text(section=None):
    hits = invocation_blocks(section if section is not None else section5_text())
    assert hits, "section 5 names no cutover_watch_checkpoint.py invocation block"
    return hits[0]


def all_invocation_blocks():
    """(doc name, block) for every pinned fence across both operator docs."""
    found = []
    for doc_path, heading in PINNED_SECTIONS:
        section = section_text(doc_path, heading)
        blocks = invocation_blocks(section)
        assert blocks, f"{doc_path.name} {heading} names no invocation block"
        found.extend((f"{doc_path.name} {heading}", block) for block in blocks)
    return found


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
    def test_every_pinned_invocation_matches_script_argparse(self):
        blocks = all_invocation_blocks()
        self.assertGreaterEqual(len(blocks), 2,
                                "both operator docs must pin an invocation block")
        code_flags = script_flags()
        self.assertEqual(tuple(code_flags), EXPECTED_FLAGS,
                         "script argparse must keep exactly the four documented flags")
        for doc_name, block in blocks:
            with self.subTest(doc=doc_name):
                doc_flags = flags_in_order(block)
                self.assertEqual(tuple(doc_flags), EXPECTED_FLAGS,
                                 f"{doc_name} invocation must list the four flags "
                                 "in argparse order")
                check_invocation(doc_flags, code_flags)

    def test_section5_states_one_checkpoint_per_call(self):
        section = section5_text()
        collapsed = re.sub(r"\s+", " ", section)
        self.assertIn("one checkpoint per call", collapsed)
        for doc_name, block in all_invocation_blocks():
            with self.subTest(doc=doc_name):
                self.assertEqual(block.count("--checkpoint"), 1,
                                 f"{doc_name} invocation records one checkpoint per call")
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

    def test_second_drifted_block_in_section_fails_named(self):
        """Guards a hits[0]-only collector: a drifted second block must fail."""
        section = section5_text()
        blocks = invocation_blocks(section)
        self.assertGreaterEqual(len(blocks), 1)
        drifted = blocks[0].replace("--expected-sha", "--sha")
        self.assertIn("--sha", flags_in_order(drifted))
        code_flags = script_flags()
        with self.assertRaisesRegex(AssertionError, r"--expected-sha|--sha"):
            for block in [*blocks, drifted]:
                check_invocation(flags_in_order(block), code_flags)

    def test_production_doc_drift_fails_named(self):
        """The production-deploy.md copy is pinned too, not just section 5."""
        prod_section = section_text(PROD_DOC, "## 48-hour watch log")
        blocks = invocation_blocks(prod_section)
        self.assertGreaterEqual(len(blocks), 1,
                                "production-deploy.md watch log names an invocation block")
        code_flags = script_flags()
        for doc_name, block in all_invocation_blocks():
            with self.subTest(doc=doc_name):
                check_invocation(flags_in_order(block), code_flags)
        drifted = blocks[0].replace("--expected-sha", "--sha")
        with self.assertRaisesRegex(AssertionError, r"--expected-sha|--sha"):
            check_invocation(flags_in_order(drifted), code_flags)


if __name__ == "__main__":
    unittest.main()

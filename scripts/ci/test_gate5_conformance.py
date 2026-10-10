"""Offline conformance: Gate 5 doc claims match scripts/gate5_outage.py.

`docs/soak-entry-gates.md` (Gate 5) documents the offline tool's exact CLI,
event vocabulary, fail-closed exit-status contract and no-live-producer
statement. Nothing enforced that the doc and the tool stay equal, so flag or
vocabulary drift would pass CI silently. This suite fails when either side
drifts; update both together.
"""

import contextlib
import io
import os
import re
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import gate5_outage  # noqa: E402

DOC = ROOT / "docs/soak-entry-gates.md"
TOOL = ROOT / "scripts/gate5_outage.py"


def gate5_section() -> str:
    """Gate 5 section text (from its heading to the next top-level heading)."""
    text = DOC.read_text(encoding="utf-8")
    marker = "## Gate 5"
    start = text.index(marker)
    rest = text[start + len(marker):]
    end = rest.find("\n## ")
    return rest if end == -1 else rest[:end]


def folded(section: str) -> str:
    """Section with line wraps collapsed, so reflow alone never fails the pin."""
    return re.sub(r"\s+", " ", section)


def run_cli(content: bytes, *args: str):
    """Run the tool's main() on a temp tick log; return (exit_code, stdout)."""
    with tempfile.TemporaryDirectory(
        dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
    ) as tmp:
        path = os.path.join(tmp, "tick-log.jsonl")
        with open(path, "wb") as handle:
            handle.write(content)
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = gate5_outage.main([path, *args])
    return code, out.getvalue()


class Gate5DocConformanceTests(unittest.TestCase):
    def test_gate5_section_names_the_tool(self):
        section = gate5_section()
        self.assertIn(
            "scripts/gate5_outage.py",
            section,
            "Gate 5 must name the offline tool it documents",
        )

    def test_invocation_shape_matches_tool(self):
        section = gate5_section()
        source = TOOL.read_text(encoding="utf-8")
        # The doc's invocation shape: script, JSONL log, interval bounds.
        for token in (
            "python3 scripts/gate5_outage.py",
            "tick-log.jsonl",
            "--interval-start",
            "--interval-end",
        ):
            with self.subTest(token=token):
                self.assertIn(
                    token,
                    section,
                    f"Gate 5 invocation shape drifted: {token!r} missing from the doc",
                )
        # The tool actually defines that shape: a log positional plus both
        # interval flags. Renaming a flag without updating the doc (or vice
        # versa) turns this suite red.
        for token in ('"log"', '"--interval-start"', '"--interval-end"'):
            with self.subTest(tool_token=token):
                self.assertIn(
                    token,
                    source,
                    f"tool CLI shape drifted: {token} missing from scripts/gate5_outage.py",
                )
        code, _ = run_cli(
            b'{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            b'{"ts": "2026-10-01T00:00:02Z", "event": "readyz_ok"}\n',
            "--interval-start",
            "2026-10-01T00:00:00Z",
            "--interval-end",
            "2026-10-01T00:00:01Z",
        )
        self.assertEqual(code, 0, "documented invocation shape must run")

    def test_event_vocabulary_matches_tool(self):
        section = gate5_section()
        # The doc names each event with its role: recovery vs outage start.
        for event in ("readyz_ok", "readyz_fail", "tick_missed"):
            with self.subTest(event=event):
                self.assertIn(
                    f"`{event}`",
                    section,
                    f"Gate 5 vocabulary drifted: `{event}` missing from the doc",
                )
        self.assertIn("recovery", section)
        self.assertIn("outage start", section)
        # The tool implements exactly that vocabulary.
        self.assertEqual(gate5_outage.RECOVERY_EVENT, "readyz_ok")
        self.assertEqual(
            tuple(gate5_outage.OUTAGE_START_EVENTS), ("readyz_fail", "tick_missed")
        )
        # The tool rejects a vocabulary swap the doc does not bless: a
        # readyz_fail carrying status 200 is UNKNOWN, never a start.
        summary = gate5_outage.summarize(
            io.StringIO(
                '{"ts": "2026-10-01T00:00:00Z", "event": "readyz_ok"}\n'
                '{"ts": "2026-10-01T00:00:10Z", "event": "readyz_fail", "status": 200}\n'
                '{"ts": "2026-10-01T00:00:20Z", "event": "readyz_ok"}\n'
            ),
            gate5_outage.parse_ts("2026-10-01T00:00:00Z"),
            gate5_outage.parse_ts("2026-10-01T00:00:01Z"),
        )
        self.assertEqual(summary["verdict"], "NOT VERIFIED")
        self.assertEqual(summary["outage_windows"], [])

    def test_fail_closed_contract_matches_tool(self):
        section = gate5_section()
        prose = folded(section)
        self.assertIn(
            "Exit status is nonzero unless the verdict is PASS",
            prose,
            "Gate 5 must document the fail-closed exit-status contract",
        )
        self.assertIn("PASS", section)
        source = TOOL.read_text(encoding="utf-8")
        self.assertIn(
            'if summary["verdict"] != "PASS"',
            source,
            "tool fail-closed guard drifted from the documented contract",
        )
        # Prove the contract, not just its spelling: PASS exits 0, every
        # other verdict exits nonzero.
        clean = (
            b'{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            b'{"ts": "2026-10-01T00:00:02Z", "event": "readyz_ok"}\n'
        )
        gapped = (
            b'{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            b'{"ts": "2026-10-01T01:00:01Z", "event": "readyz_ok"}\n'
        )
        breach = (
            b'{"ts": "2026-10-01T00:00:00Z", "event": "readyz_fail", "status": 503}\n'
            b'{"ts": "2026-10-01T00:02:00Z", "event": "readyz_ok"}\n'
        )
        interval = (
            "--interval-start",
            "2026-10-01T00:00:00Z",
            "--interval-end",
            "2026-10-01T00:00:01Z",
        )
        cases = (
            ("PASS", clean, interval, 0),
            ("NOT VERIFIED", gapped, interval, 1),
            ("NEEDS WORK", breach, interval, 1),
            ("missing interval", clean, (), 1),
        )
        for name, content, args, expected in cases:
            with self.subTest(case=name):
                code, out = run_cli(content, *args)
                self.assertEqual(code, expected)
                self.assertIn(f'"verdict": "{name}"' if name != "missing interval" else '"verdict"', out)

    def test_no_live_producer_matches_tool(self):
        section = gate5_section()
        self.assertIn(
            "there is no live producer",
            section,
            "Gate 5 must keep the no-live-producer statement",
        )
        self.assertIn(
            "committed JSONL tick",
            section,
            "Gate 5 must state the tool reads a committed JSONL tick log",
        )
        source = TOOL.read_text(encoding="utf-8")
        self.assertIn(
            "never touches staging or production",
            source,
            "tool must keep its offline-only contract",
        )
        for forbidden in ("urllib", "requests", "socket", "http.client"):
            with self.subTest(module=forbidden):
                self.assertNotIn(
                    f"import {forbidden}",
                    source,
                    f"tool gained a live dependency: {forbidden}",
                )
        # The tool opens exactly one file: the log path it was given. No
        # second open, no staging/production touch.
        self.assertEqual(source.count("open("), 1)

    def test_not_verified_caveat_keeps_its_meaning(self):
        prose = folded(gate5_section())
        self.assertIn(
            "NOT VERIFIED until a reviewed log source exists",
            prose,
            "Gate 5 must keep the NOT VERIFIED caveat with its meaning",
        )
        self.assertIn(
            "A pass verifies offline reconciliation only",
            prose,
            "Gate 5 must keep the offline-only scope of a tool PASS",
        )
        self.assertIn(
            "does not prove a deployed build",
            prose,
            "Gate 5 must keep what an offline PASS does not prove",
        )
        # The caveat is meaningful because the tool distinguishes the two:
        # a clean offline log passes, an empty one does not verify.
        start = gate5_outage.parse_ts("2026-10-01T00:00:00Z")
        end = gate5_outage.parse_ts("2026-10-01T00:00:01Z")
        passing = gate5_outage.summarize(
            io.StringIO(
                '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
                '{"ts": "2026-10-01T00:00:02Z", "event": "readyz_ok"}\n'
            ),
            start,
            end,
        )
        missing = gate5_outage.summarize(io.StringIO(""), start, end)
        self.assertEqual(passing["verdict"], "PASS")
        self.assertEqual(missing["verdict"], "NOT VERIFIED")


if __name__ == "__main__":
    unittest.main()

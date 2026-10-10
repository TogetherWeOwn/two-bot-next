"""Voice-config-apply doc invocation conformance: offline, no network or databases.

Pins `docs/voice-config-apply.md` to the `voice-config-apply` binary arg
parse (`crates/cutover/src/bin/voice_config_apply.rs`) so flag drift fails
CI, and pins the doc exit-code table to the binary's actual exits.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOC = ROOT / "docs/voice-config-apply.md"
SEQ = ROOT / "docs/cutover-sequence.md"
BIN = ROOT / "crates/cutover/src/bin/voice_config_apply.rs"
CLI = ROOT / "crates/cutover/src/cli.rs"

# The operator invocation the doc teaches: dry run plus the bound apply.
EXPECTED_DOC_FLAGS = ("--guild", "--file", "--apply", "--expect-hash")
FLAG_RE = re.compile(r"--[a-z0-9][a-z0-9-]*")
FENCE_RE = re.compile(r"```sh(.*?)```", re.DOTALL)
QUOTED_RE = re.compile(r'"([^"]+)"')


def doc_fences(doc_text=None):
    text = doc_text if doc_text is not None else DOC.read_text()
    return FENCE_RE.findall(text)


def doc_invocation_text(fences=None):
    fences = fences if fences is not None else doc_fences()
    hits = [block for block in fences if "voice-config-apply --guild" in block]
    assert hits, "voice-config-apply.md names no --guild invocation block"
    return hits[0]


def flags_in_order(text):
    """Flags in first-appearance order: the doc fence teaches the dry run
    and the bound apply in one block, so --guild/--file repeat."""
    seen = []
    for flag in FLAG_RE.findall(text):
        if flag not in seen:
            seen.append(flag)
    return seen


def binary_allowed_args(source=None):
    """Accepted `--value` keys and bare `--flag` names from the binary.

    The binary allowlists both sides just past arg parse and refuses
    anything else with exit 2, so the doc invocation must stay inside
    this set.
    """
    text = source if source is not None else BIN.read_text()
    values_match = re.search(
        r"for key in args\.values\.keys\(\) \{.*?matches!\(\s*key\.as_str\(\),\s*(.*?)\)",
        text,
        re.DOTALL,
    )
    flags_match = re.search(
        r"for flag in &args\.flags \{.*?matches!\(\s*flag\.as_str\(\),\s*(.*?)\)",
        text,
        re.DOTALL,
    )
    assert values_match, "binary allowlists no --value keys past arg parse"
    assert flags_match, "binary allowlists no bare --flag names past arg parse"
    values = tuple(QUOTED_RE.findall(values_match.group(1)))
    flags = tuple(QUOTED_RE.findall(flags_match.group(1)))
    assert values and flags, "binary arg allowlist parsed empty"
    return values, flags


def check_doc_shape(doc_flags):
    """Fail named when the doc invocation stops teaching the four flags."""
    if tuple(doc_flags) != EXPECTED_DOC_FLAGS:
        raise AssertionError(
            "voice-config-apply doc invocation drifted "
            f"(doc lists {list(doc_flags)}; "
            f"must list {list(EXPECTED_DOC_FLAGS)} in order)"
        )


def check_invocation(doc_flags, allowed):
    """Fail named when the doc invocation drifts from the binary arg parse."""
    allowed_set = {f"--{name}" for name in allowed}
    unknown = sorted(set(doc_flags) - allowed_set)
    if unknown:
        raise AssertionError(
            "voice-config-apply doc invocation drifted from the binary arg parse "
            f"(doc flags the binary refuses: {unknown}; "
            f"binary accepts: {sorted(allowed_set)})"
        )


class VoiceConfigApplyDocConformanceTests(unittest.TestCase):
    def test_doc_invocation_flags_match_binary_arg_parse(self):
        invocation = doc_invocation_text()
        doc_flags = flags_in_order(invocation)
        check_doc_shape(doc_flags)
        values, flags = binary_allowed_args()
        check_invocation(doc_flags, values + flags)

    def test_binary_still_accepts_exactly_the_documented_surface(self):
        values, flags = binary_allowed_args()
        self.assertEqual(
            tuple(values),
            ("guild", "file", "discord-base", "expect-hash"),
            "binary --value surface changed; update the doc pin",
        )
        self.assertEqual(
            tuple(flags),
            ("apply", "allow-live-guild"),
            "binary bare-flag surface changed; update the doc pin",
        )

    def test_apply_stays_bound_to_expect_hash(self):
        invocation = doc_invocation_text()
        self.assertIn("--apply", invocation)
        self.assertIn("--expect-hash", invocation)
        source = BIN.read_text()
        self.assertIn(
            "--apply needs --expect-hash",
            source,
            "--apply without --expect-hash must stay a usage refusal",
        )
        self.assertIn("--file is required", source)

    def test_live_guild_fence_matches_doc(self):
        doc = DOC.read_text()
        self.assertIn("--allow-live-guild", doc)
        source = BIN.read_text()
        self.assertIn(
            'require_guild(&args, "guild")',
            source,
            "binary must gate --guild through the live-guild fence",
        )
        self.assertIn("Refusing live guild", CLI.read_text())

    def test_doc_exit_code_table_matches_binary_exits(self):
        doc = DOC.read_text()
        collapsed = re.sub(r"\s+", " ", doc)
        table = re.search(r"Exit codes:(.*?)(?:\.|$)", collapsed)
        self.assertIsNotNone(table, "doc names no exit-code table")
        table_text = table.group(1)
        for code in ("0", "1", "2", "3"):
            self.assertRegex(
                table_text,
                rf"(?:^|\D){code}(?:\D|$)",
                f"doc exit-code table names no exit {code}",
            )
        for meaning in ("dry run", "refused", "usage", "hash mismatch"):
            self.assertIn(
                meaning,
                table_text,
                f"doc exit-code table explains no {meaning}",
            )
        self.assertIn("compare-and-swap conflict", table_text)
        source = BIN.read_text()
        self.assertIn("std::process::exit(2)", source)
        self.assertIn("std::process::exit(1)", source)
        self.assertIn("std::process::exit(3)", source)
        self.assertIn("std::process::exit(code)", source)
        # Exit 3 has exactly its two sites: reviewed-hash mismatch and the
        # compare-and-swap conflict on a changed stored configuration.
        self.assertEqual(source.count("std::process::exit(3)"), 2)
        self.assertIn("hash mismatch", BIN.parents[1].joinpath("voice_config_apply.rs").read_text())
        self.assertIn("RowNotFound", source)

    def test_cutover_sequence_names_voice_config_apply_at_v11(self):
        seq = SEQ.read_text()
        self.assertIn(
            "voice-config-apply",
            seq,
            "cutover-sequence.md never mentions the voice-config-apply tool",
        )
        v11_step = [line for line in seq.splitlines() if "voice-config-apply" in line]
        self.assertTrue(
            any("V11" in line or "voice-config-apply.md" in line for line in v11_step),
            "cutover-sequence.md pointer must sit at the V11 step",
        )

    def test_deliberate_drift_fixture_fails_named(self):
        values, flags = binary_allowed_args()
        allowed = values + flags
        # Renamed flag: --expect-hash shortened to --hash.
        with self.assertRaisesRegex(AssertionError, "--hash"):
            check_doc_shape(["--guild", "--file", "--apply", "--hash"])
        with self.assertRaisesRegex(AssertionError, "--hash"):
            check_invocation(["--guild", "--file", "--apply", "--hash"], allowed)
        # Dropped flag: --apply missing from the taught invocation.
        with self.assertRaisesRegex(AssertionError, "--apply"):
            check_doc_shape(["--guild", "--file", "--expect-hash"])
        # Added flag: --verbose never existed in arg parse.
        with self.assertRaisesRegex(AssertionError, "--verbose"):
            check_invocation([*EXPECTED_DOC_FLAGS, "--verbose"], allowed)


if __name__ == "__main__":
    unittest.main()

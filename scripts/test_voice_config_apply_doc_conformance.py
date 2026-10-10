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
LIB = ROOT / "crates/cutover/src/voice_config_apply.rs"
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


# Each exit code paired with the meaning the doc must teach for it. Codes
# and meanings are checked as pairs: a table that swaps two meanings must
# fail, not just one that drops a code or a word.
EXPECTED_EXIT_CODE_MEANINGS = (
    ("0", "dry run"),
    ("1", "refused"),
    ("2", "usage"),
    ("3", "hash mismatch"),
)


def exit_code_table_text(doc_text=None):
    text = doc_text if doc_text is not None else DOC.read_text()
    collapsed = re.sub(r"\s+", " ", text)
    table = re.search(r"Exit codes:(.*?)(?:\.|$)", collapsed)
    assert table, "doc names no exit-code table"
    return table.group(1)


def check_exit_code_pairs(table_text, pairs=EXPECTED_EXIT_CODE_MEANINGS):
    """Fail named when an exit code stops explaining its own meaning."""
    segments = [seg.strip() for seg in table_text.split(";")]
    for code, meaning in pairs:
        hits = [seg for seg in segments if re.match(rf"{code}\b", seg)]
        if not hits:
            raise AssertionError(f"doc exit-code table names no exit {code}")
        if not any(meaning in seg for seg in hits):
            raise AssertionError(
                f"doc exit-code table pairs exit {code} with no {meaning!r} "
                f"(exit {code} explains: {hits})"
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
        table_text = exit_code_table_text()
        # Codes paired with their meanings: swapping two meanings fails.
        try:
            check_exit_code_pairs(table_text)
        except AssertionError as exc:
            self.fail(str(exc))
        self.assertIn(
            "compare-and-swap conflict",
            table_text,
            "doc exit-code table explains no compare-and-swap conflict",
        )
        source = BIN.read_text()
        self.assertIn("std::process::exit(2)", source)
        self.assertIn("std::process::exit(1)", source)
        self.assertIn("std::process::exit(3)", source)
        self.assertIn("std::process::exit(code)", source)
        # Exit 3 has exactly its two sites: reviewed-hash mismatch and the
        # compare-and-swap conflict on a changed stored configuration.
        self.assertEqual(source.count("std::process::exit(3)"), 2)
        self.assertIn("hash mismatch", LIB.read_text())
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
        # Row 2.6 runs against the live guild, which the binary refuses
        # (exit 2) without the flag, so both its commands must carry it.
        command_rows = [line for line in v11_step if "--guild" in line]
        self.assertTrue(
            command_rows,
            "cutover-sequence.md V11 row teaches no voice-config-apply command",
        )
        for row in command_rows:
            self.assertEqual(
                row.count("--allow-live-guild"),
                2,
                "cutover-sequence.md V11 row must pass --allow-live-guild "
                "to both the dry run and the apply (live-guild fence is exit 2)",
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
        # Swapped exit-code meanings: every code and word is still present,
        # but paired wrong. Exits 1 and 2 trade meanings here, exits 2 and
        # 3 trade meanings in the second fixture.
        swap_1_2 = (
            "0 dry run, no changes or applied; 1 usage or failed; "
            "2 refused or live-guild fence; 3 hash mismatch or "
            "compare-and-swap conflict"
        )
        with self.assertRaisesRegex(AssertionError, "exit 1"):
            check_exit_code_pairs(swap_1_2)
        swap_2_3 = (
            "0 dry run; 1 refused or failed; 2 hash mismatch or fence; "
            "3 usage or compare-and-swap conflict"
        )
        with self.assertRaisesRegex(AssertionError, "exit 2"):
            check_exit_code_pairs(swap_2_3)


if __name__ == "__main__":
    unittest.main()

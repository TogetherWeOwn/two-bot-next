"""Acceptance tests: bot live-data lint must fail on hard-coded Discord facts.

The Builder card implements a new CI lint, ``scripts/ci/check-live-data.py``,
that blocks hard-coded Discord facts in production Rust source (host decision
D14, SPEC card 7 bot half). These tests pin the lint's observable behavior
with synthetic trees only and must turn GREEN without editing this file. The
Builder builds on this card's branch and never edits this file.

Required gate contract (mirrors ``check-src-snowflakes.py``):

- ``occurrences(source)`` scans one Rust source string and returns a list of
  ``(line, kind, value, source_line)`` tuples. ``line`` is 1-based, ``kind``
  is exactly ``"snowflake"`` (17-20 digit channel/guild id literal),
  ``"channel_name"`` (after-party/lobby name literal) or ``"schedule"``
  (day-of-week + time string), ``value`` is the matched fact text, and
  ``source_line`` is the stripped source line.
- ``check(root, allowlist)`` walks ``crates/*/src/**/*.rs`` and
  ``src/**/*.rs`` under ``root``, matches occurrences against ``allowlist``
  entries shaped ``{"path", "value", "source", "reason"}`` (same
  occurrence-scoped, path-scoped, stale-detecting semantics as the snowflake
  gate), and returns a list of error strings shaped
  ``"<path>:<line>: ...<value>..."``.
- Only production literals count: ``//`` and ``/* */`` comments, exact
  ``#[cfg(test)]`` inline modules and ``#![cfg(test)]`` files are excluded,
  exactly like the snowflake gate.
- The single bot constant holding the after-party template is expressed as
  the one allowlist entry for that literal; any second occurrence fails.

Card-bullet mapping:

- "Fixture with a hard-coded channel id literal fails the lint":
  ``ChannelIdTests``. The snowflake gate already scans production source,
  but this lint must flag ids itself: the Builder may reuse the snowflake
  tokenizer internally, not skip the kind.
- "Fixture with an after-party name literal outside the single bot constant
  fails": ``AfterPartyNameTests``.
- "Fixture with a schedule string ('Sunday 8pm' style) fails; clean fixture
  passes": ``ScheduleStringTests``.

Base state: the lint module does not exist, so every lint-dependent test
FAILS (assertion, not import error) with instructions naming the missing
module. ``FixtureWellFormednessTests`` passes before and after: it proves
each dirty fixture carries exactly its own marker and the clean fixture
carries none, so the RED comes from the missing lint, never from a
malformed fixture.
"""

import importlib.util
import os
from pathlib import Path
import re
import tempfile
import unittest

GATE_FILENAME = "check-live-data.py"
GATE_DISPLAY = "scripts/ci/check-live-data.py"

try:
    _SPEC = importlib.util.spec_from_file_location(
        "live_data", Path(__file__).with_name(GATE_FILENAME)
    )
    if _SPEC is None or _SPEC.loader is None:
        raise ImportError(f"cannot load {GATE_DISPLAY}")
    live_data = importlib.util.module_from_spec(_SPEC)
    _SPEC.loader.exec_module(live_data)
    GATE_IMPORT_ERROR = None
except Exception as exc:
    # RED state: the lint module does not exist yet. Every lint-dependent
    # test reports this through require_gate instead of erroring at import.
    live_data = None
    GATE_IMPORT_ERROR = exc


def require_gate(test):
    """Return the lint module or fail with Builder instructions."""
    if live_data is None:
        test.fail(
            f"live-data lint is missing ({GATE_IMPORT_ERROR!r}). Builder: "
            f"create {GATE_DISPLAY} exposing occurrences(source) with kinds "
            "'snowflake' | 'channel_name' | 'schedule' and "
            "check(root, allowlist); see this file's docstring."
        )
    for attr in ("occurrences", "check"):
        if not hasattr(live_data, attr):
            test.fail(
                f"live-data lint lacks required {attr}(). Builder: add "
                f"{attr} to {GATE_DISPLAY} per this file's docstring."
            )
    return live_data


FIXTURE_PATH = "crates/bot/src/live_data_gate_fixture.rs"
FIXTURE_PATH_TWO = "crates/bot/src/live_data_gate_fixture_two.rs"
CONSTANT_PATH = "crates/bot/src/event_voice.rs"

NUM_ID = "123456789012345678"
STR_ID = "123456789012345679"
AFTER_PARTY = "after-party · launch-night"
SCHEDULE_ONE = "Sunday 8pm"
SCHEDULE_TWO = "Sunday at 8pm"

SOURCE_ID_NUM = f"pub const VOICE_CATEGORY_ID: u64 = {NUM_ID};\n"
SOURCE_ID_STR = f'pub const CHANNEL_JUMP: &str = "{STR_ID}";\n'
SOURCE_AFTER_PARTY = f'pub const ROOM_NAME: &str = "{AFTER_PARTY}";\n'
SOURCE_SCHEDULE_ONE = f'pub const GAME_NIGHT: &str = "{SCHEDULE_ONE}";\n'
SOURCE_SCHEDULE_TWO = f'pub const GAME_NIGHT_AT: &str = "{SCHEDULE_TWO}";\n'

CLEAN = (
    "pub fn event_voice_category() -> Option<u64> {\n"
    '    std::env::var("TWO_EVENT_VOICE_CATEGORY_ID")\n'
    "        .ok()\n"
    "        .and_then(|raw| raw.parse().ok())\n"
    "}\n"
    "\n"
    'pub const TBD_REASON: &str = "opens_soon";\n'
    "pub const CREATE_WINDOW_MINUTES: u64 = 15;\n"
)

WEEKDAY_TIME = re.compile(
    r"\b(?:monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b"
    r"[^\"'\n]{0,8}\b\d{1,2}(?::\d{2})?\s*(?:am|pm)\b",
    re.IGNORECASE,
)
LONG_DIGITS = re.compile(r"\d{17,}")


def write(root, relative, text):
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def scratch():
    return tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))


class ChannelIdTests(unittest.TestCase):
    def test_numeric_channel_id_is_a_snowflake_occurrence(self):
        gate = require_gate(self)
        self.assertEqual(
            gate.occurrences(SOURCE_ID_NUM),
            [(1, "snowflake", NUM_ID, SOURCE_ID_NUM.strip())],
        )

    def test_string_channel_id_is_a_snowflake_occurrence(self):
        gate = require_gate(self)
        self.assertEqual(
            gate.occurrences(SOURCE_ID_STR),
            [(1, "snowflake", STR_ID, SOURCE_ID_STR.strip())],
        )

    def test_unallowlisted_channel_id_fails_the_check(self):
        gate = require_gate(self)
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, SOURCE_ID_NUM)
            errors = gate.check(root, [])
            self.assertEqual(len(errors), 1)
            self.assertTrue(errors[0].startswith(f"{FIXTURE_PATH}:"), errors)
            self.assertIn(NUM_ID, errors[0])


class AfterPartyNameTests(unittest.TestCase):
    def test_after_party_literal_is_a_channel_name_occurrence(self):
        gate = require_gate(self)
        found = gate.occurrences(SOURCE_AFTER_PARTY)
        self.assertEqual(len(found), 1)
        line, kind, value, source = found[0]
        self.assertEqual(line, 1)
        self.assertEqual(kind, "channel_name")
        self.assertIn("after-party", value)
        self.assertEqual(source, SOURCE_AFTER_PARTY.strip())

    def test_after_party_outside_the_constant_fails_the_check(self):
        gate = require_gate(self)
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, SOURCE_AFTER_PARTY)
            errors = gate.check(root, [])
            self.assertEqual(len(errors), 1)
            self.assertTrue(errors[0].startswith(f"{FIXTURE_PATH}:"), errors)
            self.assertIn("after-party", errors[0])

    def test_single_allowlisted_constant_passes_but_a_duplicate_fails(self):
        gate = require_gate(self)
        with scratch() as temp:
            root = Path(temp)
            write(root, CONSTANT_PATH, SOURCE_AFTER_PARTY)
            _, _, value, source = gate.occurrences(SOURCE_AFTER_PARTY)[0]
            allowance = [{
                "path": CONSTANT_PATH,
                "value": value,
                "source": source,
                "reason": "the single bot constant holding the after-party template",
            }]
            self.assertEqual(gate.check(root, allowance), [])
            write(root, FIXTURE_PATH, SOURCE_AFTER_PARTY)
            errors = gate.check(root, allowance)
            self.assertEqual(len(errors), 1)
            self.assertTrue(errors[0].startswith(f"{FIXTURE_PATH}:"), errors)


class ScheduleStringTests(unittest.TestCase):
    def test_sunday_evening_strings_are_schedule_occurrences(self):
        gate = require_gate(self)
        for source in (SOURCE_SCHEDULE_ONE, SOURCE_SCHEDULE_TWO):
            with self.subTest(source=source):
                found = gate.occurrences(source)
                self.assertEqual(len(found), 1)
                line, kind, value, origin = found[0]
                self.assertEqual(line, 1)
                self.assertEqual(kind, "schedule")
                self.assertIn("Sunday", value)
                self.assertEqual(origin, source.strip())

    def test_unallowlisted_schedule_strings_fail_the_check(self):
        gate = require_gate(self)
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, SOURCE_SCHEDULE_ONE)
            write(root, FIXTURE_PATH_TWO, SOURCE_SCHEDULE_TWO)
            errors = gate.check(root, [])
            self.assertEqual(len(errors), 2)
            self.assertTrue(
                any(e.startswith(f"{FIXTURE_PATH}:") for e in errors), errors
            )
            self.assertTrue(
                any(e.startswith(f"{FIXTURE_PATH_TWO}:") for e in errors), errors
            )
            self.assertTrue(all("Sunday" in e for e in errors), errors)

    def test_clean_fixture_passes(self):
        gate = require_gate(self)
        self.assertEqual(gate.occurrences(CLEAN), [])
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, CLEAN)
            self.assertEqual(gate.check(root, []), [])


class ProductionLiteralSemanticsTests(unittest.TestCase):
    def test_comments_are_not_live_data(self):
        gate = require_gate(self)
        source = (
            f"// {NUM_ID} {AFTER_PARTY} {SCHEDULE_ONE}\n"
            f"/* {STR_ID} {AFTER_PARTY} {SCHEDULE_TWO} */\n"
            "pub const VERSION: u64 = 1;\n"
        )
        self.assertEqual(gate.occurrences(source), [])

    def test_cfg_test_code_is_not_live_data(self):
        gate = require_gate(self)
        source = (
            "#[cfg(test)]\n"
            "mod tests {\n"
            f'    const ID: &str = "{NUM_ID}";\n'
            f'    const NAME: &str = "{AFTER_PARTY}";\n'
            f'    const WHEN: &str = "{SCHEDULE_ONE}";\n'
            "}\n"
            "pub const VERSION: u64 = 1;\n"
        )
        self.assertEqual(gate.occurrences(source), [])
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, "#![cfg(test)]\n" + SOURCE_ID_NUM)
            self.assertEqual(gate.check(root, []), [])


class AllowlistHygieneTests(unittest.TestCase):
    def test_stale_allowance_fails(self):
        gate = require_gate(self)
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, CLEAN)
            errors = gate.check(root, [{
                "path": FIXTURE_PATH,
                "value": NUM_ID,
                "source": SOURCE_ID_NUM.strip(),
                "reason": "acceptance fixture",
            }])
            self.assertEqual(len(errors), 1)
            self.assertIn("stale allowance", errors[0])

    def test_allowance_requires_a_reason(self):
        gate = require_gate(self)
        with scratch() as temp:
            root = Path(temp)
            write(root, FIXTURE_PATH, SOURCE_ID_NUM)
            for reason in ("", "   "):
                with self.subTest(reason=reason), self.assertRaises(ValueError):
                    gate.check(root, [{
                        "path": FIXTURE_PATH,
                        "value": NUM_ID,
                        "source": SOURCE_ID_NUM.strip(),
                        "reason": reason,
                    }])

    def test_allowance_rejects_escaping_paths(self):
        gate = require_gate(self)
        for path in ("../outside.rs", "src/../../outside.rs"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                gate.check(Path("."), [{
                    "path": path,
                    "value": NUM_ID,
                    "source": SOURCE_ID_NUM.strip(),
                    "reason": "acceptance fixture",
                }])


class FixtureWellFormednessTests(unittest.TestCase):
    """Controls: pass before and after; prove the fixtures are single-issue."""

    def test_dirty_fixtures_carry_exactly_their_own_marker(self):
        self.assertIn(NUM_ID, SOURCE_ID_NUM)
        self.assertIn(STR_ID, SOURCE_ID_STR)
        self.assertIn(AFTER_PARTY, SOURCE_AFTER_PARTY)
        self.assertIsNotNone(WEEKDAY_TIME.search(SOURCE_SCHEDULE_ONE))
        self.assertIsNotNone(WEEKDAY_TIME.search(SOURCE_SCHEDULE_TWO))
        for source in (SOURCE_AFTER_PARTY, SOURCE_SCHEDULE_ONE, SOURCE_SCHEDULE_TWO):
            self.assertIsNone(LONG_DIGITS.search(source), source)
        for source in (SOURCE_ID_NUM, SOURCE_ID_STR, SOURCE_SCHEDULE_ONE, SOURCE_SCHEDULE_TWO):
            self.assertNotIn("after-party", source.lower())
        for source in (SOURCE_ID_NUM, SOURCE_ID_STR, SOURCE_AFTER_PARTY):
            self.assertIsNone(WEEKDAY_TIME.search(source), source)

    def test_clean_fixture_carries_no_marker(self):
        self.assertIsNone(LONG_DIGITS.search(CLEAN))
        self.assertNotIn("after-party", CLEAN.lower())
        self.assertIsNone(WEEKDAY_TIME.search(CLEAN))
        self.assertIn("TWO_EVENT_VOICE_CATEGORY_ID", CLEAN)


if __name__ == "__main__":
    unittest.main()

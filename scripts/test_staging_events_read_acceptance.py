#!/usr/bin/env python3
"""Acceptance matrix: staging event evidence must reject malformed CSV and
impossible or out-of-window timestamps. Offline only; no database, no network.

The query window in scripts/staging_events_read.py is SQL ``BETWEEN``, so both
window bounds are inclusive. A returned ``recorded_at`` is evidence only when
it is a real UTC calendar/clock instant inside that inclusive window; a regex
shape is not. Malformed psql CSV (for example an unterminated quoted field)
must produce the existing bounded read failure: exit 1 on the rows read, no
artifact, no step summary, and no raw row/member/database value echoed.

These tests reuse the fake-psql/full-main seams from
test_staging_events_read.py plus the public ``build_artifact`` /
``artifact_errors`` contract. They do not prescribe where the implementation
performs the semantic checks.
"""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_events_read as reader
import test_staging_events_read as base

WINDOW = ("2026-10-07T14:00:00.000Z", "2026-10-07T14:15:00.000Z")
# Ten real minutes across the 2026 (non-leap) February/March boundary. The
# impossible instants 2026-02-29 and 2026-02-30 sort lexically inside this
# window, so only a real calendar parse can reject them.
FEBRUARY_WINDOW = ("2026-02-28T23:55:00.000Z", "2026-03-01T00:05:00.000Z")
# The same span in leap year 2024, where 2024-02-29 is a real day.
LEAP_WINDOW = ("2024-02-28T23:55:00.000Z", "2024-02-29T00:05:00.000Z")


def artifact(window, *recorded_ats):
    return reader.build_artifact(
        window[0], window[1],
        [{"event_type": "member_join", "recorded_at": value} for value in recorded_ats])


class RowsReadRejectionTests(unittest.TestCase):
    """Full-main seam: bad rows output fails bounded, writes nothing, echoes nothing."""

    def assert_read_failed(self, rows, forbidden=()):
        fixture = base.Fixture(self, rows=rows)
        code, out, err = fixture.run()
        self.assertEqual(code, 1, err)
        self.assertEqual(out, "")
        self.assertTrue(err.startswith("failed:"), err)
        # The bounded failure is deliberate fixed vocabulary, not the generic
        # exception handler leaking a csv error name or a traceback.
        self.assertNotIn("_csv", err)
        self.assertNotIn("Traceback", err)
        self.assertFalse(fixture.output.exists(), "no artifact may be written")
        self.assertFalse(fixture.summary.exists(), "no summary may be written")
        for value in (base.MEMBER, base.PASSWORD, *forbidden):
            self.assertNotIn(value, out + err, "diagnostics must not echo raw values")

    def test_impossible_clock_and_calendar_instants_are_rejected(self):
        for label, recorded_at in {
            "second 60 inside the window": "2026-10-07T14:05:60.000Z",
            "minute 99": "2026-10-07T14:99:00.000Z",
            "minute 60": "2026-10-07T14:60:00.000Z",
            "hour 24": "2026-10-07T24:01:00.000Z",
            "day 32": "2026-10-32T14:01:00.000Z",
            "day 00": "2026-10-00T14:01:00.000Z",
            "month 13": "2026-13-07T14:01:00.000Z",
            "month 00": "2026-00-07T14:01:00.000Z",
            "february 30": "2026-02-30T14:01:00.000Z",
            "february 29 in a non-leap year": "2026-02-29T14:01:00.000Z",
        }.items():
            with self.subTest(case=label):
                self.assert_read_failed(f"member_join,{recorded_at}\n", forbidden=(recorded_at,))

    def test_rows_outside_the_inclusive_sql_window_are_rejected(self):
        for label, recorded_at in {
            "one millisecond before the start": "2026-10-07T13:59:59.999Z",
            "one millisecond after the end": "2026-10-07T14:15:00.001Z",
            "same day well before": "2026-10-07T00:00:00.000Z",
            "same day well after": "2026-10-07T23:59:59.999Z",
            "previous day": "2026-10-06T14:05:00.000Z",
            "next day": "2026-10-08T14:05:00.000Z",
        }.items():
            with self.subTest(case=label):
                self.assert_read_failed(f"member_join,{recorded_at}\n", forbidden=(recorded_at,))

    def test_one_bad_row_among_valid_rows_rejects_the_whole_read(self):
        rows = ("member_join,2026-10-07T14:01:00.000Z\n"
                "first_message,2026-10-07T14:15:00.001Z\n"
                "second_message,2026-10-07T14:03:00.000Z\n")
        self.assert_read_failed(rows, forbidden=("2026-10-07T14:15:00.001Z",))

    def test_an_out_of_window_row_beyond_the_truncation_cap_still_rejects(self):
        # The 61st returned row is the truncation witness and never reaches
        # the artifact; it must still be a plausible query result.
        rows = base.rows_csv(60) + "first_message,2026-10-07T14:15:00.001Z\n"
        self.assert_read_failed(rows, forbidden=("2026-10-07T14:15:00.001Z",))

    def test_malformed_csv_is_rejected(self):
        good_time = "2026-10-07T14:01:00.000Z"
        for label, rows in {
            # A permissive reader silently returns the clean field here.
            "unterminated quoted field at end of output": f'member_join,"{good_time}',
            "unterminated quoted first field at end of output": f'"member_join,{good_time}',
            "unterminated quote swallowing the next row":
                f'member_join,"{good_time}\nfirst_message,2026-10-07T14:02:00.000Z\n',
            "characters after a closing quote": f'"member_join"x,{good_time}\n',
            "space after a closing quote": f'"member_join" ,{good_time}\n',
            "unterminated quoted field in a later row":
                f"member_join,{good_time}\n" + 'first_message,"2026-10-07T14:02:00.000Z',
        }.items():
            with self.subTest(case=label):
                self.assert_read_failed(rows, forbidden=(good_time,))

    def test_an_unterminated_identity_row_stops_before_the_rows_query(self):
        # Permissive parsing currently reads this as the exact expected
        # identity. Strict parsing must refuse it before any rows are read,
        # whichever bounded failure vocabulary the implementation maps it to.
        fixture = base.Fixture(self, rows=base.rows_csv(1),
                               identity='"two_bot_events_ro","two_bot')
        code, out, err = fixture.run()
        self.assertNotEqual(code, 0, err)
        self.assertEqual(out, "")
        self.assertNotIn("Traceback", err)
        self.assertNotIn("_csv", err)
        self.assertFalse(fixture.output.exists())
        self.assertFalse(fixture.summary.exists())
        self.assertEqual(len(fixture.calls()), 1, "the rows query must not run")
        self.assertNotIn(base.PASSWORD, out + err)
        self.assertNotIn(base.MEMBER, out + err)


class RowsReadControlTests(unittest.TestCase):
    """Valid evidence keeps working, including the SQL's inclusive bounds."""

    def assert_read_ok(self, rows, expect_rows=None, expect_truncated=False):
        fixture = base.Fixture(self, rows=rows)
        code, out, err = fixture.run()
        self.assertEqual((code, err), (0, ""))
        written = fixture.artifact()
        self.assertEqual(reader.artifact_errors(written), [])
        self.assertEqual(written["truncated"], expect_truncated)
        if expect_rows is not None:
            self.assertEqual([row["recorded_at"] for row in written["rows"]], expect_rows)
            self.assertEqual([row["ordinal"] for row in written["rows"]],
                             list(range(1, len(expect_rows) + 1)))
        return written

    def test_valid_quoted_csv_is_preserved(self):
        self.assert_read_ok('"member_join","2026-10-07T14:01:00.000Z"\n',
                            expect_rows=["2026-10-07T14:01:00.000Z"])

    def test_rows_exactly_on_both_inclusive_boundaries_are_kept(self):
        self.assert_read_ok(f"member_join,{WINDOW[0]}\nfirst_message,{WINDOW[1]}\n",
                            expect_rows=[WINDOW[0], WINDOW[1]])

    def test_non_chronological_rows_keep_their_query_order(self):
        # The SQL orders by id, not by recorded_at; no chronological-order
        # requirement may be invented for the artifact.
        self.assert_read_ok("member_join,2026-10-07T14:10:00.000Z\n"
                            "first_message,2026-10-07T14:02:00.000Z\n",
                            expect_rows=["2026-10-07T14:10:00.000Z",
                                         "2026-10-07T14:02:00.000Z"])

    def test_zero_rows_still_reads_clean(self):
        written = self.assert_read_ok("", expect_rows=[])
        self.assertEqual(written["row_count"], 0)

    def test_the_truncation_witness_is_preserved(self):
        written = self.assert_read_ok(base.rows_csv(61), expect_truncated=True)
        self.assertEqual((written["row_count"], len(written["rows"])), (60, 60))


class ArtifactContractTests(unittest.TestCase):
    """artifact_errors must not certify impossible or out-of-window evidence."""

    def test_impossible_row_instants_are_errors(self):
        for label, candidate in {
            "second 60": artifact(WINDOW, "2026-10-07T14:05:60.000Z"),
            "minute 99 after a valid row": artifact(
                WINDOW, "2026-10-07T14:01:00.000Z", "2026-10-07T14:99:00.000Z"),
            "february 30 lexically inside its window": artifact(
                FEBRUARY_WINDOW, "2026-02-30T00:00:00.000Z"),
            "february 29 in a non-leap year": artifact(
                FEBRUARY_WINDOW, "2026-02-29T00:00:00.000Z"),
        }.items():
            with self.subTest(case=label):
                self.assertTrue(reader.artifact_errors(candidate))

    def test_out_of_window_rows_are_errors_on_either_side(self):
        for label, candidate in {
            "one millisecond before the start": artifact(WINDOW, "2026-10-07T13:59:59.999Z"),
            "one millisecond after the end": artifact(WINDOW, "2026-10-07T14:15:00.001Z"),
            "second kept row out of window": artifact(
                WINDOW, "2026-10-07T14:01:00.000Z", "2026-10-07T13:00:00.000Z"),
        }.items():
            with self.subTest(case=label):
                self.assertTrue(reader.artifact_errors(candidate))

    def test_unordered_or_impossible_window_bounds_are_errors(self):
        for label, candidate in {
            "reversed window": artifact((WINDOW[1], WINDOW[0])),
            "equal bounds": artifact((WINDOW[0], WINDOW[0])),
            "impossible start": artifact(("2026-10-07T14:00:60.000Z", WINDOW[1])),
            "impossible end": artifact((WINDOW[0], "2026-10-07T14:15:60.000Z")),
            "february 30 start": artifact(("2026-02-30T23:55:00.000Z", FEBRUARY_WINDOW[1])),
        }.items():
            with self.subTest(case=label):
                self.assertTrue(reader.artifact_errors(candidate))

    def test_valid_boundaries_and_ordering_stay_clean(self):
        for label, candidate in {
            "row on the start bound": artifact(WINDOW, WINDOW[0]),
            "row on the end bound": artifact(WINDOW, WINDOW[1]),
            "non-chronological rows inside the window": artifact(
                WINDOW, "2026-10-07T14:10:00.000Z", "2026-10-07T14:02:00.000Z"),
            "no rows": artifact(WINDOW),
            "leap day in a leap year": artifact(LEAP_WINDOW, "2024-02-29T00:00:00.000Z"),
        }.items():
            with self.subTest(case=label):
                self.assertEqual(reader.artifact_errors(candidate), [])


if __name__ == "__main__":
    unittest.main()

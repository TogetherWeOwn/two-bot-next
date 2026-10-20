"""Acceptance matrix for the container-stdout evidence contract (offline only).

These tests pin three properties of scripts/container_stdout_read.py through
its CLI surface (`main` with an injected transport; no socket is ever opened):

* identity-sensitive redaction cannot be bypassed by encoding a value as a
  JSON integer instead of a string, while bounded numeric counters keep
  their values;
* every event instant is validated against the requested query window
  (both boundaries inclusive, millisecond precision) before a packet is
  printed;
* the requested result-count limit is enforced as a validity check, never
  by silently truncating or relabeling an over-limit response.

Malformed timestamps, out-of-window events and over-limit responses must
fail the whole read with the existing fixed vocabulary and print no partial
JSON. Valid responses keep the current oldest-first ordering, empty-result
meaning and conservative `possibly_truncated` signal (true at exactly the
requested limit).

This file is the contract: satisfy it by changing the reader, its docs and
its CI wiring, not by editing these tests.
"""

import copy
from datetime import datetime, timedelta, timezone
import fnmatch
import json
from pathlib import Path
import re
import socket
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import container_stdout_read as reader
from test_container_stdout_read import (
    ACCOUNT,
    END,
    RESPONSE,
    SERVICE,
    FakeTransport,
    run_main,
)

ROOT = Path(__file__).resolve().parent.parent
THIS_FILE = Path(__file__).name

WINDOW_END = datetime(2026, 10, 11, 12, 20, 0, tzinfo=timezone.utc)
WINDOW_START = WINDOW_END - timedelta(minutes=5)
MID = datetime(2026, 10, 11, 12, 18, 0, tzinfo=timezone.utc)

SNOWFLAKE_INT = 987654321098765432  # 18 digits: Discord-snowflake shaped
SNOWFLAKE_TEXT = str(SNOWFLAKE_INT)
CANONICAL_RUN_ID = "1791721100000000000-1"
# Assembled at run time so no scanner mistakes the sentinel for a real id.
SENTINEL = "sentinel-" + "7" * 18
BAD_RESPONSE_LINE = "container_stdout_read: read failed (bad_response)"

_MISSING = object()


def ms(moment):
    return int(moment.timestamp() * 1000)


def make_event(moment=MID, message="probe line", raw_timestamp=_MISSING, **source):
    event = {
        "dataset": "containers",
        "source": {"message": message, **source},
        "$metadata": {"service": SERVICE, "level": "info", "message": message},
    }
    event["timestamp"] = ms(moment) if raw_timestamp is _MISSING else raw_timestamp
    return event


def answer_with(events):
    answer = copy.deepcopy(RESPONSE)
    answer["result"]["events"]["events"] = events
    return answer


class AcceptanceBase(unittest.TestCase):
    def setUp(self):
        # The matrix is fully offline: any real connection attempt is a bug.
        refused = AssertionError("network access is disabled in this suite")
        for patcher in (mock.patch.object(socket.socket, "connect", side_effect=refused),
                        mock.patch.object(socket, "create_connection", side_effect=refused)):
            patcher.start()
            self.addCleanup(patcher.stop)

    def cli(self, events, limit=None, minutes=5):
        transport = FakeTransport(answer_with(events))
        argv = ["--service", SERVICE, "--end", END, "--minutes", str(minutes),
                "--account-id", ACCOUNT]
        if limit is not None:
            argv += ["--limit", str(limit)]
        code, out, err = run_main(argv, transport)
        return code, out, err, transport

    def packet(self, events, **kwargs):
        code, out, err, _ = self.cli(events, **kwargs)
        self.assertEqual((code, err), (0, ""), out)
        return json.loads(out), out

    def assert_failed_closed(self, events, hidden=(), **kwargs):
        code, out, err, transport = self.cli(events, **kwargs)
        self.assertEqual(code, 1)
        self.assertEqual(out, "", "a failed read must print no partial JSON")
        self.assertEqual(err.strip(), BAD_RESPONSE_LINE)
        for marker in hidden:
            self.assertNotIn(marker, out)
            self.assertNotIn(marker, err)
        return transport


class ScalarRedactionAcceptance(AcceptanceBase):
    def test_integer_run_id_is_redacted_like_a_noncanonical_string(self):
        for value in (SNOWFLAKE_INT, 1791721100000000000, 7):
            with self.subTest(value=value):
                packet, out = self.packet([make_event(run_id=value)])
                self.assertEqual(packet["events"][0]["run_id"], "[redacted]")
                self.assertEqual(packet["redactions"], 1)
                if value > 10**12:
                    self.assertNotIn(str(value), out)

    def test_canonical_string_run_id_is_retained(self):
        packet, _ = self.packet([make_event(run_id=CANONICAL_RUN_ID)])
        self.assertEqual(packet["events"][0]["run_id"], CANONICAL_RUN_ID)
        self.assertEqual(packet["redactions"], 0)

    def test_string_run_id_without_the_pid_suffix_is_redacted(self):
        packet, out = self.packet([make_event(run_id=SNOWFLAKE_TEXT)])
        self.assertEqual(packet["events"][0]["run_id"], "[redacted]")
        self.assertEqual(packet["redactions"], 1)
        self.assertNotIn(SNOWFLAKE_TEXT, out)

    def test_integer_snowflake_in_any_kept_field_becomes_the_id_marker(self):
        for key in ("code", "count", "revision", "step", "error_class"):
            with self.subTest(key=key):
                packet, out = self.packet([make_event(**{key: SNOWFLAKE_INT})])
                self.assertEqual(packet["events"][0][key], "[id]")
                self.assertEqual(packet["redactions"], 1)
                self.assertNotIn(SNOWFLAKE_TEXT, out)

    def test_integer_and_string_snowflakes_have_parity_in_the_same_field(self):
        from_int, _ = self.packet([make_event(code=SNOWFLAKE_INT)])
        from_str, _ = self.packet([make_event(code=SNOWFLAKE_TEXT)])
        self.assertEqual(from_int["events"][0]["code"], "[id]")
        self.assertEqual(from_int["events"][0]["code"], from_str["events"][0]["code"])
        self.assertEqual(from_int["redactions"], from_str["redactions"])

    def test_snowflake_digit_boundaries_for_integers(self):
        cases = (
            (12345678901234, 12345678901234, 0),      # 14 digits: a counter
            (123456789012345, "[id]", 1),             # 15 digits: snowflake floor
            (1234567890123456789012, "[id]", 1),      # 22 digits: snowflake ceiling
        )
        for value, expected, redactions in cases:
            with self.subTest(digits=len(str(value))):
                packet, _ = self.packet([make_event(revision=value)])
                self.assertEqual(packet["events"][0]["revision"], expected)
                self.assertEqual(packet["redactions"], redactions)

    def test_bounded_numeric_counters_keep_their_values(self):
        packet, _ = self.packet([make_event(count=0, code=8080, revision=42)])
        event = packet["events"][0]
        self.assertEqual((event["count"], event["code"], event["revision"]), (0, 8080, 42))
        self.assertEqual(packet["redactions"], 0)

    def test_negative_bounded_counter_keeps_its_value(self):
        packet, _ = self.packet([make_event(code=-1)])
        self.assertEqual(packet["events"][0]["code"], -1)
        self.assertEqual(packet["redactions"], 0)

    def test_integer_snowflake_as_the_message_becomes_the_id_marker(self):
        event = make_event()
        event["source"]["message"] = SNOWFLAKE_INT
        event["$metadata"]["message"] = SNOWFLAKE_INT
        packet, out = self.packet([event])
        self.assertEqual(packet["events"][0]["message"], "[id]")
        self.assertEqual(packet["redactions"], 1)
        self.assertNotIn(SNOWFLAKE_TEXT, out)

    def test_each_integer_identity_value_counts_as_a_redaction(self):
        packet, out = self.packet([make_event(run_id=SNOWFLAKE_INT, code=SNOWFLAKE_INT)])
        event = packet["events"][0]
        self.assertEqual(event["run_id"], "[redacted]")
        self.assertEqual(event["code"], "[id]")
        self.assertEqual(packet["redactions"], 2)
        self.assertNotIn(SNOWFLAKE_TEXT, out)


class WindowAcceptance(AcceptanceBase):
    def test_events_on_both_boundaries_are_kept_oldest_first(self):
        events = [  # the query answers newest-first
            make_event(WINDOW_END, "at end"),
            make_event(MID, "in the middle"),
            make_event(WINDOW_START, "at start"),
        ]
        packet, _ = self.packet(events)
        self.assertEqual(packet["window"], {"start": "2026-10-11T12:15:00.000Z",
                                            "end": "2026-10-11T12:20:00.000Z"})
        self.assertEqual([e["message"] for e in packet["events"]],
                         ["at start", "in the middle", "at end"])
        self.assertEqual([e["ts"] for e in packet["events"]],
                         ["2026-10-11T12:15:00.000Z", "2026-10-11T12:18:00.000Z",
                          "2026-10-11T12:20:00.000Z"])
        self.assertEqual(packet["event_count"], 3)
        self.assertFalse(packet["possibly_truncated"])

    def test_events_one_millisecond_inside_both_boundaries_are_kept(self):
        events = [
            make_event(WINDOW_END - timedelta(milliseconds=1), "just before end"),
            make_event(WINDOW_START + timedelta(milliseconds=1), "just after start"),
        ]
        packet, _ = self.packet(events)
        self.assertEqual([e["message"] for e in packet["events"]],
                         ["just after start", "just before end"])

    def test_event_one_millisecond_before_the_window_fails_closed(self):
        event = make_event(WINDOW_START - timedelta(milliseconds=1),
                           "early " + SENTINEL)
        self.assert_failed_closed([event], hidden=(SENTINEL, "early"))

    def test_event_one_millisecond_after_the_window_fails_closed(self):
        event = make_event(WINDOW_END + timedelta(milliseconds=1),
                           "late " + SENTINEL)
        self.assert_failed_closed([event], hidden=(SENTINEL, "late"))

    def test_events_well_outside_the_window_fail_closed(self):
        for moment, label in ((WINDOW_END - timedelta(minutes=10), "ten minutes old"),
                              (WINDOW_END + timedelta(minutes=5), "after the window")):
            with self.subTest(label=label):
                event = make_event(moment, label + " " + SENTINEL)
                self.assert_failed_closed([event], hidden=(SENTINEL, label))

    def test_one_out_of_window_event_fails_the_whole_packet(self):
        marker = "partialpacketmarker"
        events = [
            make_event(MID, marker),
            make_event(WINDOW_START - timedelta(seconds=1), "stale"),
        ]
        # No partial success: even the in-window event's text stays unpublished.
        self.assert_failed_closed(events, hidden=(marker, "stale"))

    def test_malformed_timestamps_fail_closed(self):
        bad = {
            "string": make_event(raw_timestamp="2026-10-11T12:18:00Z"),
            "float": make_event(raw_timestamp=float(ms(MID))),
            "bool": make_event(raw_timestamp=True),
            "null": make_event(raw_timestamp=None),
            "overflow": make_event(raw_timestamp=10**30),
        }
        missing = make_event()
        del missing["timestamp"]
        bad["missing"] = missing
        for label, event in bad.items():
            with self.subTest(label=label):
                event["source"]["message"] = "broken " + SENTINEL
                event["$metadata"]["message"] = "broken " + SENTINEL
                self.assert_failed_closed([event], hidden=(SENTINEL,))

    def test_validation_uses_the_requested_window_from_the_one_query(self):
        event = make_event(WINDOW_START - timedelta(milliseconds=1), "early")
        transport = self.assert_failed_closed([event], hidden=("early",))
        self.assertEqual(len(transport.calls), 1, "validation must not re-query")
        method, path, body = transport.calls[0]
        self.assertEqual(method, "POST")
        self.assertTrue(path.endswith("/workers/observability/telemetry/query"))
        self.assertEqual(body["timeframe"], {"from": ms(WINDOW_START), "to": ms(WINDOW_END)})


class LimitAcceptance(AcceptanceBase):
    def valid_events(self, count):
        base = WINDOW_START + timedelta(seconds=30)
        newest_first = [make_event(base + timedelta(seconds=10 * i), f"line {i}")
                        for i in range(count)]
        return list(reversed(newest_first))

    def test_more_events_than_the_requested_limit_fails_closed(self):
        for limit, count in ((1, 2), (2, 3), (4, 5)):
            with self.subTest(limit=limit, count=count):
                events = self.valid_events(count)
                events[0]["source"]["message"] = "over " + SENTINEL
                events[0]["$metadata"]["message"] = "over " + SENTINEL
                transport = self.assert_failed_closed(events, hidden=(SENTINEL,), limit=limit)
                self.assertEqual(len(transport.calls), 1)
                self.assertEqual(transport.calls[0][2]["limit"], limit)

    def test_exact_limit_is_kept_and_flagged_possibly_truncated(self):
        packet, _ = self.packet(self.valid_events(2), limit=2)
        self.assertEqual(packet["event_count"], 2)
        self.assertEqual(packet["limit"], 2)
        self.assertTrue(packet["possibly_truncated"])
        self.assertEqual([e["message"] for e in packet["events"]], ["line 0", "line 1"])

    def test_under_the_limit_is_not_flagged_possibly_truncated(self):
        packet, _ = self.packet(self.valid_events(2), limit=3)
        self.assertEqual(packet["event_count"], 2)
        self.assertFalse(packet["possibly_truncated"])

    def test_empty_result_at_the_smallest_limit_is_a_valid_zero(self):
        packet, _ = self.packet([], limit=1)
        self.assertEqual((packet["event_count"], packet["events"]), (0, []))
        self.assertFalse(packet["possibly_truncated"])


class ContractSurfaceAcceptance(AcceptanceBase):
    def test_startup_diagnostics_doc_states_the_enforced_output_contract(self):
        doc = (ROOT / "docs" / "startup-diagnostics.md").read_text()
        section = doc.split("## Reading container stdout", 1)[1].split("\n## ", 1)[0]
        expectations = {
            "numeric values are redacted too":
                r"(integer|numeric)[\s\S]{0,160}?(redact|scrub)",
            "out-of-window events fail the read":
                r"outside[\s\S]{0,120}?window[\s\S]{0,160}?(fail|error|reject)",
            "over-limit responses fail the read":
                r"(more events than|over|exceed)[\s\S]{0,120}?limit[\s\S]{0,160}?(fail|error|reject)",
        }
        for label, pattern in expectations.items():
            with self.subTest(label=label):
                self.assertRegex(section, re.compile(pattern, re.IGNORECASE))

    def test_ci_runs_this_acceptance_file(self):
        workflow = (ROOT / ".github" / "workflows" / "check.yml").read_text()
        patterns = []
        for line in workflow.splitlines():
            if "unittest" in line and "discover" in line and "-s scripts" in line \
                    and "scripts/ci" not in line:
                found = re.search(r"-p\s+'([^']+)'|-p\s+\"([^\"]+)\"", line)
                patterns.append(next(g for g in found.groups() if g) if found else "test*.py")
        self.assertTrue(patterns, "no scripts unittest step found in check.yml")
        self.assertTrue(
            any(fnmatch.fnmatch(THIS_FILE, pattern) for pattern in patterns),
            f"no check.yml unittest pattern runs {THIS_FILE}: {patterns}",
        )

    def test_reader_module_keeps_the_documented_caps(self):
        # The transport/response fences are part of the contract being kept,
        # not knobs the fix may widen.
        self.assertEqual(reader.MAX_LIMIT, 100)
        self.assertEqual(reader.MAX_MINUTES, 30)
        self.assertEqual(reader.MAX_RESPONSE_BYTES, 4 * 1024 * 1024)


if __name__ == "__main__":
    unittest.main()

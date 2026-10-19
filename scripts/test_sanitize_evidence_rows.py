"""Offline raw-row conversion fixtures; no network or database access."""

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sanitize_evidence_rows as sanitizer  # noqa: E402

SCRIPT = Path(__file__).with_name("sanitize_evidence_rows.py")
KEY = "100000000000000001:900000000000001111:member_join:RAW_KEY_SENTINEL"


def row(**changes):
    return {"idempotency_key": KEY, "event_type": "member_join",
            "recorded_at": "2026-10-09T20:00:01+00:00", **changes}


class SanitizerTests(unittest.TestCase):
    def test_raw_query_array_becomes_reconciler_input_without_keys(self):
        result = sanitizer.sanitize(json.dumps([row(), row(event_type="first_message")]))
        self.assertEqual(result, {"rows": [
            {"ordinal": 0, "event_type": "member_join", "recorded_at": "2026-10-09T20:00:01.000Z"},
            {"ordinal": 1, "event_type": "first_message", "recorded_at": "2026-10-09T20:00:01.000Z"},
        ], "truncated": False})
        text = json.dumps(result)
        for forbidden in (KEY, "idempotency_key", "100000000000000001", "900000000000001111"):
            self.assertNotIn(forbidden, text)

    def test_every_known_kind_stays_intact(self):
        result = sanitizer.sanitize(json.dumps([row(event_type=kind)
                                               for kind in sorted(sanitizer.EVENT_TYPES)]))
        self.assertEqual([r["event_type"] for r in result["rows"]], sorted(sanitizer.EVENT_TYPES))

    def test_postgres_session_timezone_offsets_normalize_to_utc(self):
        for timestamp in ("2026-10-09T21:00:01+01:00",
                          "2026-10-09T15:30:01-04:30",
                          "2026-10-10T01:00:01+05:00"):
            result = sanitizer.sanitize(json.dumps([row(recorded_at=timestamp)]))
            self.assertEqual(result["rows"][0]["recorded_at"], "2026-10-09T20:00:01.000Z")

    def test_empty_read_remains_empty_not_a_fake_receipt(self):
        self.assertEqual(sanitizer.sanitize("[]"), {"rows": [], "truncated": False})

    def test_cap_is_conservative_even_when_exactly_full(self):
        for count in (59, 60, 61, 70):
            result = sanitizer.sanitize(json.dumps([row()] * count))
            self.assertEqual(len(result["rows"]), min(count, 60))
            self.assertEqual(result["truncated"], count >= 60)
            self.assertEqual([r["ordinal"] for r in result["rows"]], list(range(min(count, 60))))

    def test_all_rows_are_validated_even_past_output_cap(self):
        with self.assertRaises(sanitizer.Refused):
            sanitizer.sanitize(json.dumps([row()] * 60 + [row(event_type="RAW_KEY_SENTINEL")]))

    def test_bad_rows_are_refused(self):
        for bad in ("not json", "{}", "null", "[null]", "[{}]", json.dumps([
                {"event_type": "member_join", "recorded_at": "2026-10-09T20:00:01Z"}]),
                *[json.dumps([r]) for r in (
                    row(member_id="900000000000001111"), row(idempotency_key=None),
                    row(event_type="RAW_KEY_SENTINEL"), row(event_type=[]),
                    row(recorded_at=None), row(recorded_at="RAW_KEY_SENTINEL"),
                    row(recorded_at="2026-10-09T20:00:01"),
                    row(recorded_at="0001-01-01T00:00:00+01:00"),
                )]):
            with self.subTest(bad=bad), self.assertRaises(sanitizer.Refused):
                sanitizer.sanitize(bad)

    def drive(self, raw):
        env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
        return subprocess.run([sys.executable, str(SCRIPT)], input=raw,
                              capture_output=True, env=env, check=False, timeout=10)

    def test_stdin_cli_only_writes_sanitized_json(self):
        done = self.drive(json.dumps([row()]).encode())
        self.assertEqual(done.returncode, 0)
        self.assertEqual(json.loads(done.stdout)["rows"][0]["ordinal"], 0)
        self.assertEqual(done.stderr, b"")
        self.assertNotIn(KEY.encode(), done.stdout)

    def test_stdin_cli_failure_writes_no_json_or_raw_details(self):
        for raw in (json.dumps([row(event_type=KEY)]).encode(), KEY.encode(), b"\xff"):
            done = self.drive(raw)
            self.assertEqual(done.returncode, 2)
            self.assertEqual(done.stdout, b"")
            self.assertNotIn(KEY.encode(), done.stderr)
            self.assertNotIn(b"900000000000001111", done.stderr)

    def test_stdin_byte_cap_fails_closed(self):
        done = self.drive(b" " * (sanitizer.MAX_INPUT_BYTES + 1))
        self.assertEqual(done.returncode, 2)
        self.assertEqual(done.stdout, b"")


if __name__ == "__main__":
    unittest.main()

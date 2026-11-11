"""Offline regression tests for inert production-hold receipt parsing."""

import json
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import production_hold_receipts as receipts


REPORT = {
    "last_promoted_sha": "a" * 40,
    "last_promoted_at": "2026-01-02T00:00:00Z",
    "promotion_run_id": "123",
    "promotion_attempt": 1,
    "promotion_mode": "deploy",
    "main_sha": "b" * 40,
    "observed_at": "2026-01-01T12:00:00Z",
    "held_since": "2026-01-01T06:00:00Z",
    "reason": "held",
    "run_url": "https://example.invalid/run",
}


def envelope(kind, value):
    return f"content\n<!-- production-hold-{kind}: {value} -->"


class History:
    def __init__(self, comments=()):
        self.comments = comments

    def pages(self, path):
        return self.comments if path.endswith("/comments") else []


class ReceiptParsingTest(unittest.TestCase):
    def test_deep_json_is_rejected_in_both_envelopes_and_callers(self):
        # Construct JSON directly: the decoder, not the encoder, hits its limit.
        nested = '{"nested":' + '[' * 2000 + '0' + ']' * 2000 + '}'
        with self.assertRaises(RecursionError):
            json.loads(nested)
        for kind, pattern in (("observation", receipts.OBSERVATION),
                              ("recovery", receipts.RECOVERY)):
            with self.subTest(kind=kind):
                text = envelope(kind, nested)
                self.assertIsNone(receipts.verified_content(text, pattern))
                self.assertFalse(receipts.stale_observation(text, REPORT))
                self.assertFalse(receipts.recovery_receipt(text, REPORT))
        issue = {"number": 1, "updated_at": "2026-01-03T00:00:00Z",
                 "body": envelope("observation", nested)}
        self.assertFalse(receipts.stale_activity(History(), issue, REPORT))

        old = dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z")
        issue["body"] = receipts.body(old) + "\n" + receipts.observation(old)
        comment = {"updated_at": "2026-01-03T00:00:00Z",
                   "body": envelope("recovery", nested)}
        self.assertFalse(receipts.stale_activity(History([comment]), issue, REPORT))

    def test_valid_receipts_and_invalid_controls(self):
        old = dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z")
        observation = receipts.body(old) + "\n" + receipts.observation(old)
        recovery = receipts.recovery_body(REPORT, "example/repo")
        self.assertTrue(receipts.stale_observation(observation, REPORT))
        self.assertTrue(receipts.recovery_receipt(recovery, REPORT))
        for kind, pattern in (("observation", receipts.OBSERVATION),
                              ("recovery", receipts.RECOVERY)):
            for value in ('{"x":}', '{"x":1,"x":2}', '{"content_sha256":"wrong"}'):
                with self.subTest(kind=kind, value=value):
                    self.assertIsNone(receipts.verified_content(envelope(kind, value), pattern))

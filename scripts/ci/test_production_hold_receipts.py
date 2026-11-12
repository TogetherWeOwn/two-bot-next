"""Offline regression tests for inert production-hold receipt parsing."""

import json
import subprocess
import sys
import types
import unittest
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import incident_issue
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


def receipt(module, report):
    return module.body(report) + "\n" + module.observation(report)


def change_metadata(text, **changes):
    match = receipts.OBSERVATION.search(text)
    value = json.loads(match[1])
    value.update(changes)
    return text[:match.start(1)] + json.dumps(value, sort_keys=True) + text[match.end(1):]


def stale_issue(text):
    return {"number": 1, "updated_at": "2026-01-03T00:00:00Z", "body": text}


class ReceiptParsingTest(unittest.TestCase):
    def test_observation_integrity_and_lifecycle(self):
        old = dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z")
        valid = receipt(receipts, old)
        self.assertTrue(receipts.stale_observation(valid, REPORT))
        self.assertTrue(receipts.stale_activity(History(), stale_issue(valid), REPORT))
        current = dict(old, last_promoted_at=REPORT["last_promoted_at"],
                       held_since=REPORT["last_promoted_at"], observed_at=REPORT["last_promoted_at"])
        self.assertFalse(receipts.stale_observation(receipt(receipts, current), REPORT))
        changed = dict(old, main_sha="c" * 40)
        self.assertTrue(receipts.stale_observation(receipt(receipts, changed), REPORT))
        body, marker = valid.split("\n<!--", 1)
        mixed = receipts.body(changed) + "\n<!--" + marker
        legacy_value = {key: old[key] for key in receipts.OBSERVATION_FIELDS}
        legacy_value["content_sha256"] = receipts.content_digest(body)
        legacy = body + "\n<!-- production-hold-observation: " + json.dumps(legacy_value) + " -->"
        tampered = [change_metadata(valid, last_promoted_at="2025-12-31T00:00:00Z"),
                    change_metadata(valid, observed_at="2026-01-01T13:00:00Z"),
                    valid.replace("Skip/failure reason: held", "Skip/failure reason: edited"),
                    mixed, legacy, change_metadata(valid, version=99)]
        # Even a recomputed unkeyed digest cannot excuse inconsistent duplicated fields.
        match = receipts.OBSERVATION.search(tampered[0])
        metadata = json.loads(match[1])
        metadata.pop("envelope_sha256")
        metadata["envelope_sha256"] = receipts.observation_digest(body, metadata)
        tampered.append(body + "\n<!-- production-hold-observation: " + json.dumps(metadata) + " -->")
        for text in tampered:
            with self.subTest(text=text):
                self.assertFalse(receipts.stale_observation(text, REPORT))
                self.assertFalse(receipts.stale_activity(History(), stale_issue(text), REPORT))
                comment = {"updated_at": "2026-01-03T00:00:00Z", "body": text}
                self.assertFalse(receipts.stale_activity(History([comment]), stale_issue(valid), REPORT))

    def test_sink_envelopes_preserve_v2_integrity(self):
        old = dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z")
        valid = receipt(receipts, old)
        issue_body = valid + "\n\n" + incident_issue.marker(receipts.KEY) + "\n"
        comment_body = "Still failing.\n\n" + valid
        self.assertTrue(receipts.stale_observation(issue_body, REPORT))
        self.assertTrue(receipts.stale_observation(comment_body, REPORT))
        comment = {"updated_at": "2026-01-03T00:00:00Z", "body": comment_body}
        self.assertTrue(receipts.stale_activity(History([comment]), stale_issue(issue_body), REPORT))
        for text in (issue_body + "extra", comment_body + "extra",
                     change_metadata(issue_body, observed_at="2026-01-01T13:00:00Z"),
                     change_metadata(comment_body, observed_at="2026-01-01T13:00:00Z")):
            with self.subTest(text=text):
                self.assertFalse(receipts.stale_observation(text, REPORT))

    def test_lifecycle_timestamp_overflow_retains_error_contract(self):
        class EventHistory(History):
            def pages(self, path):
                return [{"id": 1, "event": "reopened", "created_at": raw}] if path.endswith("/events") else []

        valid = receipt(receipts, dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z"))
        for raw in ("0001-01-01T00:00:00+01:00", "9999-12-31T23:59:59-01:00"):
            with self.subTest(raw=raw):
                with self.assertRaisesRegex(incident_issue.IncidentError, "production hold timestamp is unreadable"):
                    receipts.stale_activity(History(), stale_issue(valid), dict(REPORT, last_promoted_at=raw))
                with self.assertRaisesRegex(incident_issue.IncidentError, "production hold timestamp is unreadable"):
                    receipts.stale_activity(EventHistory(), stale_issue(valid), REPORT)

    def test_timestamp_bounds_and_receipt_rejection(self):
        bad = ("0001-01-01T00:00:00+01:00", "9999-12-31T23:59:59-01:00",
               "nonsense", "2026-01-01T00:00:00")
        old = dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z")
        for raw in bad:
            with self.subTest(raw=raw):
                with self.assertRaisesRegex(incident_issue.IncidentError, "production hold timestamp is unreadable"):
                    receipts.moment(raw)
                self.assertFalse(receipts.stale_observation(receipt(receipts, dict(old, observed_at=raw)), REPORT))
        self.assertEqual(receipts.moment("2026-01-01T01:00:00+01:00"),
                         datetime(2026, 1, 1, tzinfo=timezone.utc))
        self.assertEqual(receipts.moment("2026-01-01T00:00:00Z"),
                         datetime(2026, 1, 1, tzinfo=timezone.utc))

    def test_pinned_prefix_exhibits_both_original_defects(self):
        source = subprocess.check_output(
            ["git", "show", "280c9630e9618a913da3c725ebc2a5ad81367f82:scripts/production_hold_receipts.py"],
            cwd=ROOT, text=True)
        before = types.ModuleType("receipt_before_fix")
        exec(compile(source, "receipt_before_fix", "exec"), before.__dict__)
        old = dict(REPORT, last_promoted_at="2026-01-01T00:00:00Z")
        tampered = change_metadata(receipt(before, old), last_promoted_at="2025-12-31T00:00:00Z")
        self.assertTrue(before.stale_observation(tampered, REPORT))
        self.assertTrue(before.stale_activity(History(), stale_issue(tampered), REPORT))
        with self.assertRaises(OverflowError):
            before.moment("0001-01-01T00:00:00+01:00")
        with self.assertRaises(OverflowError):
            before.stale_observation(receipt(before, dict(old, observed_at="9999-12-31T23:59:59-01:00")), REPORT)

    def test_deep_json_is_rejected_in_both_envelopes_and_callers(self):
        # Construct JSON directly: the decoder, not the encoder, hits its limit.
        nested = '{"nested":' + '[' * 10000 + '0' + ']' * 10000 + '}'
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

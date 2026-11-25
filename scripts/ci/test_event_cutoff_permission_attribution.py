"""Replay the operator's evidence comparison with synthetic records only."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from test_event_cutoff_release_positions import PINS, VERIFIER, capture_runner, load_fixture


REHEARSAL = load_fixture("verify_release_positions")
with patch.dict(sys.modules, verify_release_positions=REHEARSAL):
    compare_attribution = load_fixture("verify_handoff").compare_attribution


class EventCutoffPermissionAttributionTest(unittest.TestCase):
    def setUp(self):
        # Reviewed pins, private request attribution (including the separate
        # Worker evidence), and redacted HTTP responses each start independently.
        self.releases = {
            "current": {
                **PINS["current"], "worker_version": "00000000-0000-4000-8000-000000000001",
                "guild_id": "1", "bot_id": "2",
            },
            "retained": {
                **PINS["retained"], "worker_version": "00000000-0000-4000-8000-000000000002",
                "guild_id": "1", "bot_id": "3",
            },
        }
        self.requests = [
            {
                "nonce": "a" * 32, "position": "current", "image_digest": "sha256:" + "a" * 64,
                "source_revision": "1" * 40, "worker_version": "00000000-0000-4000-8000-000000000001",
                "guild_id": "1", "bot_id": "2",
            },
            {
                "nonce": "b" * 32, "position": "retained", "image_digest": "sha256:" + "b" * 64,
                "source_revision": "2" * 40, "worker_version": "00000000-0000-4000-8000-000000000002",
                "guild_id": "1", "bot_id": "3",
            },
        ]
        counts = {"channels": 1, "create_events": 0, "manage_events": 0, "either_event_permission": 0}
        permissions = {
            "owner_or_admin": False, "guild_create_events": False, "guild_manage_events": False,
            "voice": counts, "stage": counts,
        }
        self.receipts = {
            "current": {
                "v": 1, "scope_nonce": "a" * 32,
                "observed_at_unix_seconds": 100, "permissions": permissions,
            },
            "retained": {
                "v": 1, "scope_nonce": "b" * 32,
                "observed_at_unix_seconds": 100, "permissions": permissions,
            },
        }

    def compare(self, position):
        return compare_attribution(
            position, self.releases[position], self.receipts[position], self.requests,
        )

    def test_synthetic_pair_matches_reviewed_positions(self):
        REHEARSAL.verify(PINS, capture_runner(PINS), 0)
        for position in self.releases:
            with self.subTest(position=position):
                self.assertEqual(self.compare(position), "matches release position")

    def test_retained_response_cannot_prove_current(self):
        self.receipts["current"] = self.receipts["retained"]
        self.assertEqual(self.compare("current"), "attribution mismatch: position")

    def test_same_revision_wrong_digest_cannot_prove_current(self):
        self.requests[0]["image_digest"] = self.requests[1]["image_digest"]
        self.assertEqual(self.compare("current"), "attribution mismatch: image_digest")

    def test_conflicting_worker_revision_or_scope_cannot_prove_retained(self):
        for field, wrong_value in (
            ("worker_version", self.requests[0]["worker_version"]),
            ("source_revision", self.requests[0]["source_revision"]),
            ("guild_id", "4"),
            ("bot_id", self.requests[0]["bot_id"]),
        ):
            with self.subTest(field=field):
                original = self.requests[1][field]
                self.requests[1][field] = wrong_value
                self.assertEqual(self.compare("retained"), f"attribution mismatch: {field}")
                self.requests[1][field] = original

    def test_receipt_alone_leaves_attribution_gap(self):
        self.requests.pop(0)
        self.assertEqual(self.compare("current"), "attribution gap: signed request")

    def test_case_changed_nonce_leaves_attribution_gap(self):
        receipt = self.receipts["current"]
        receipt["scope_nonce"] = receipt["scope_nonce"].upper()
        self.assertEqual(self.compare("current"), "attribution gap: signed request")

    def test_reused_nonce_leaves_attribution_gap(self):
        self.requests[1]["nonce"] = self.requests[0]["nonce"].upper()
        self.assertEqual(self.compare("current"), "attribution gap: signed request")

    def test_missing_identity_field_leaves_attribution_gap(self):
        for field in ("image_digest", "source_revision", "worker_version", "guild_id", "bot_id"):
            with self.subTest(field=field):
                original = self.requests[1].pop(field)
                self.assertEqual(self.compare("retained"), f"attribution gap: {field}")
                self.requests[1][field] = original

    def run_handoff(self, exit_code=0, *, rehearsal_pins=PINS):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pins = root / "pins.json"
            transcript = root / "cutoff.log"
            attribution = root / "attribution.json"
            pins.write_text(json.dumps(self.releases), encoding="utf-8")
            capture = capture_runner(rehearsal_pins)
            REHEARSAL.verify(rehearsal_pins, capture, 0)
            transcript.write_text(capture, encoding="utf-8")
            attribution.write_text(json.dumps({
                "receipts": self.receipts, "requests": self.requests,
            }), encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(VERIFIER.with_name("verify_handoff.py")),
                 "--pins", str(pins), "--transcript", str(transcript),
                 "--runner-exit-code", str(exit_code), "--attribution", str(attribution)],
                capture_output=True, text=True, check=False, timeout=10,
            )

    def assert_handoff_unresolved(self, result, reason):
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, f"Cutoff handoff unresolved: {reason}\n")

    def test_handoff_cli_matches_complete_pair(self):
        result = self.run_handoff()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "Cutoff handoff evidence matches both release positions\n")
        self.assertEqual(result.stderr, "")

    def test_handoff_cli_leaves_missing_position_evidence_unresolved(self):
        for index, position in enumerate(("current", "retained")):
            with self.subTest(position=position, missing="request"):
                request = self.requests.pop(index)
                self.assert_handoff_unresolved(
                    self.run_handoff(), f"{position} attribution gap: signed request",
                )
                self.requests.insert(index, request)
            with self.subTest(position=position, missing="receipt"):
                receipt = self.receipts.pop(position)
                self.assert_handoff_unresolved(
                    self.run_handoff(), f"{position} attribution gap: expected a v1 receipt with a valid nonce",
                )
                self.receipts[position] = receipt

    def test_handoff_cli_leaves_swapped_position_evidence_unresolved(self):
        self.receipts = {"current": self.receipts["retained"], "retained": self.receipts["current"]}
        self.assert_handoff_unresolved(self.run_handoff(), "current attribution mismatch: position")

    def test_handoff_cli_leaves_failed_rehearsal_unresolved(self):
        self.assert_handoff_unresolved(self.run_handoff(exit_code=1), "Rehearsal exited 1")

    def test_handoff_cli_leaves_mismatched_rehearsal_unresolved(self):
        swapped = {"current": PINS["retained"], "retained": PINS["current"]}
        self.assert_handoff_unresolved(
            self.run_handoff(rehearsal_pins=swapped),
            "Rehearsal current image differs from selected pin",
        )


if __name__ == "__main__":
    unittest.main()
